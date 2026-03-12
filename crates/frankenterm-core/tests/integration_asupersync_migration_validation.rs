//! Asupersync migration validation suite (ft-1memj.32).
//!
//! Verifies that the tokio/smol → asupersync migration is complete and
//! correct across all layers: source-level import hygiene, runtime_compat
//! surface contract completeness, async boundary contracts, and
//! cross-module runtime consistency.

use frankenterm_core::runtime_compat::{
    self, CompatRuntime, RuntimeBuilder, SURFACE_CONTRACT_V1, SurfaceDisposition,
};
use frankenterm_core::runtime_compat_surface_guard::{
    SurfaceGuardReport, allowed_raw_runtime_files, standard_guard_checks, standard_surface_entries,
};

// =========================================================================
// 1. Surface Contract V1 structural invariants
// =========================================================================

#[test]
fn surface_contract_v1_is_non_empty() {
    assert!(
        !SURFACE_CONTRACT_V1.is_empty(),
        "SURFACE_CONTRACT_V1 should contain entries"
    );
}

#[test]
fn surface_contract_v1_all_entries_have_rationale() {
    for entry in SURFACE_CONTRACT_V1 {
        assert!(
            !entry.rationale.is_empty(),
            "entry '{}' missing rationale",
            entry.api
        );
    }
}

#[test]
fn surface_contract_v1_api_names_are_unique() {
    let mut names: Vec<&str> = SURFACE_CONTRACT_V1.iter().map(|e| e.api).collect();
    let original_len = names.len();
    names.sort();
    names.dedup();
    assert_eq!(
        names.len(),
        original_len,
        "duplicate API names in SURFACE_CONTRACT_V1"
    );
}

#[test]
fn surface_contract_v1_replace_entries_have_replacement() {
    for entry in SURFACE_CONTRACT_V1 {
        if matches!(entry.disposition, SurfaceDisposition::Replace) {
            assert!(
                entry.replacement.is_some(),
                "Replace-disposition entry '{}' should specify a replacement path",
                entry.api
            );
        }
    }
}

#[test]
fn surface_contract_v1_keep_entries_need_no_replacement() {
    for entry in SURFACE_CONTRACT_V1 {
        if matches!(entry.disposition, SurfaceDisposition::Keep) {
            // Keep entries may optionally have a replacement, but it's not required.
            // This test documents that Keep entries exist and are valid.
            assert!(
                !entry.api.is_empty(),
                "Keep-disposition entry should have a non-empty API name"
            );
        }
    }
}

#[test]
fn surface_contract_v1_covers_core_primitives() {
    let api_names: Vec<&str> = SURFACE_CONTRACT_V1.iter().map(|e| e.api).collect();

    let expected_core = [
        "RuntimeBuilder",
        "CompatRuntime::block_on",
        "sleep",
        "timeout",
    ];

    for expected in &expected_core {
        assert!(
            api_names.iter().any(|name| name.contains(expected)),
            "SURFACE_CONTRACT_V1 should cover core primitive '{expected}'"
        );
    }
}

// =========================================================================
// 2. Surface guard alignment with contract
// =========================================================================

#[test]
fn surface_guard_entries_match_contract_count() {
    let guard_entries = standard_surface_entries();
    assert_eq!(
        guard_entries.len(),
        SURFACE_CONTRACT_V1.len(),
        "guard entries should mirror SURFACE_CONTRACT_V1 length"
    );
}

#[test]
fn surface_guard_entries_all_have_known_disposition() {
    for entry in &standard_surface_entries() {
        let valid = ["Keep", "Replace", "Retire"].contains(&entry.disposition.as_str());
        assert!(
            valid,
            "guard entry '{}' has unexpected disposition '{}'",
            entry.api_name, entry.disposition
        );
    }
}

#[test]
fn surface_guard_checks_match_entry_count() {
    let checks = standard_guard_checks();
    let entries = standard_surface_entries();
    assert_eq!(
        checks.len(),
        entries.len(),
        "guard checks should have one per surface entry"
    );
}

// =========================================================================
// 3. Allowed raw-runtime files policy
// =========================================================================

#[test]
fn allowed_raw_runtime_files_is_minimal() {
    let allowed = allowed_raw_runtime_files();
    // Only runtime_compat.rs and cx.rs should touch raw runtime APIs
    assert!(
        allowed.len() <= 3,
        "allowed raw-runtime files should be a small, explicit set; got {}",
        allowed.len()
    );
}

#[test]
fn allowed_raw_runtime_files_contains_runtime_compat() {
    let allowed = allowed_raw_runtime_files();
    assert!(
        allowed.contains(&"runtime_compat.rs"),
        "runtime_compat.rs must be in the allowed list"
    );
}

#[test]
fn allowed_raw_runtime_files_contains_cx() {
    let allowed = allowed_raw_runtime_files();
    assert!(
        allowed.contains(&"cx.rs"),
        "cx.rs must be in the allowed list"
    );
}

// =========================================================================
// 4. Runtime builder and CompatRuntime trait
// =========================================================================

#[test]
fn runtime_builder_current_thread_builds_successfully() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("current_thread runtime should build");
    let result = runtime.block_on(async { 42 });
    assert_eq!(result, 42);
}

#[test]
fn runtime_builder_multi_thread_builds_successfully() {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("multi_thread runtime should build");
    let result = runtime.block_on(async { "hello" });
    assert_eq!(result, "hello");
}

#[test]
fn block_on_propagates_panics() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async { panic!("test panic") })
    }));
    assert!(result.is_err(), "block_on should propagate panics");
}

// =========================================================================
// 5. Async primitive contracts via runtime_compat
// =========================================================================

fn run_async<F: std::future::Future<Output = ()>>(f: F) {
    let rt = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(f);
}

#[test]
fn sleep_completes() {
    run_async(async {
        runtime_compat::sleep(std::time::Duration::from_millis(1)).await;
    });
}

#[test]
fn timeout_succeeds_for_fast_future() {
    run_async(async {
        let result = runtime_compat::timeout(std::time::Duration::from_secs(5), async { 99 }).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 99);
    });
}

#[test]
fn timeout_returns_err_on_expiry() {
    run_async(async {
        let result = runtime_compat::timeout(
            std::time::Duration::from_millis(1),
            runtime_compat::sleep(std::time::Duration::from_secs(60)),
        )
        .await;
        assert!(result.is_err(), "timeout should expire for slow future");
    });
}

#[test]
fn mpsc_channel_send_recv() {
    run_async(async {
        let (tx, mut rx) = runtime_compat::mpsc::channel(8);
        runtime_compat::mpsc_send(&tx, 42).await.unwrap();
        let val = runtime_compat::mpsc_recv_option(&mut rx).await;
        assert_eq!(val, Some(42));
    });
}

#[test]
fn mpsc_channel_closed_returns_none() {
    run_async(async {
        let (tx, mut rx) = runtime_compat::mpsc::channel::<i32>(8);
        drop(tx);
        let val = runtime_compat::mpsc_recv_option(&mut rx).await;
        assert_eq!(val, None);
    });
}

#[test]
fn watch_channel_send_recv() {
    run_async(async {
        let (tx, mut rx) = runtime_compat::watch::channel(0);
        tx.send(7).unwrap();
        let val = runtime_compat::watch_borrow_and_update_clone(&mut rx);
        assert_eq!(val, 7);
    });
}

#[test]
fn watch_channel_has_changed_detects_updates() {
    run_async(async {
        let (tx, rx) = runtime_compat::watch::channel(0);
        tx.send(1).unwrap();
        assert!(runtime_compat::watch_has_changed(&rx));
    });
}

#[test]
fn task_spawn_and_join() {
    run_async(async {
        let handle = runtime_compat::task::spawn(async { 100 });
        let result = handle.await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 100);
    });
}

#[test]
fn task_spawn_blocking_completes() {
    run_async(async {
        let result = runtime_compat::spawn_blocking(|| {
            std::thread::sleep(std::time::Duration::from_millis(1));
            "done"
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "done");
    });
}

#[test]
fn mutex_lock_and_read() {
    run_async(async {
        let m = runtime_compat::Mutex::new(42);
        let guard = m.lock().await;
        assert_eq!(*guard, 42);
    });
}

#[test]
fn rwlock_concurrent_reads() {
    run_async(async {
        let rw = runtime_compat::RwLock::new(99);
        let r1 = rw.read().await;
        let r2 = rw.read().await;
        assert_eq!(*r1, 99);
        assert_eq!(*r2, 99);
    });
}

#[test]
fn semaphore_acquire_release() {
    run_async(async {
        let sem = runtime_compat::Semaphore::new(2);
        let _p1 = sem.acquire().await.unwrap();
        let _p2 = sem.acquire().await.unwrap();
        assert_eq!(sem.available_permits(), 0);
    });
}

// =========================================================================
// 6. Cross-module runtime consistency
// =========================================================================

#[test]
fn runtime_compat_and_surface_guard_agree_on_entry_names() {
    let contract_apis: Vec<&str> = SURFACE_CONTRACT_V1.iter().map(|e| e.api).collect();
    let guard_apis: Vec<String> = standard_surface_entries()
        .iter()
        .map(|e| e.api_name.clone())
        .collect();

    assert_eq!(
        contract_apis.len(),
        guard_apis.len(),
        "contract and guard should have same number of entries"
    );

    for (contract, guard) in contract_apis.iter().zip(guard_apis.iter()) {
        assert_eq!(
            *contract, guard,
            "API name mismatch between contract and guard"
        );
    }
}

#[test]
fn runtime_compat_and_surface_guard_agree_on_dispositions() {
    let contract_dispositions: Vec<&str> = SURFACE_CONTRACT_V1
        .iter()
        .map(|e| match e.disposition {
            SurfaceDisposition::Keep => "Keep",
            SurfaceDisposition::Replace => "Replace",
            SurfaceDisposition::Retire => "Retire",
        })
        .collect();
    let guard_dispositions: Vec<String> = standard_surface_entries()
        .iter()
        .map(|e| e.disposition.clone())
        .collect();

    for (i, (contract_d, guard_d)) in contract_dispositions
        .iter()
        .zip(guard_dispositions.iter())
        .enumerate()
    {
        assert_eq!(
            *contract_d, guard_d,
            "disposition mismatch at index {i}: contract={contract_d}, guard={guard_d}"
        );
    }
}

// =========================================================================
// 7. Guard report finalization contracts
// =========================================================================

#[test]
fn guard_report_empty_is_compliant() {
    let mut report = SurfaceGuardReport::new("migration-validation", 0);
    report.finalize();
    assert!(
        report.overall_compliant,
        "empty report (no regressions, no checks) should be compliant"
    );
    assert!((report.compliance_rate - 1.0).abs() < f64::EPSILON);
}

#[test]
fn guard_report_all_compliant_checks() {
    let mut report = SurfaceGuardReport::new("migration-validation", 0);
    for check in standard_guard_checks() {
        report.add_guard_check(check);
    }
    report.finalize();
    // Standard checks should all be compliant (wrappers exist)
    let all_compliant = report.guard_checks.iter().all(|c| c.compliant);
    assert_eq!(report.overall_compliant, all_compliant);
}

// =========================================================================
// 8. Disposition distribution sanity
// =========================================================================

#[test]
fn surface_contract_has_keep_entries() {
    let keep_count = SURFACE_CONTRACT_V1
        .iter()
        .filter(|e| matches!(e.disposition, SurfaceDisposition::Keep))
        .count();
    assert!(
        keep_count >= 4,
        "expected at least 4 Keep entries, got {keep_count}"
    );
}

#[test]
fn surface_contract_has_replace_entries() {
    let replace_count = SURFACE_CONTRACT_V1
        .iter()
        .filter(|e| matches!(e.disposition, SurfaceDisposition::Replace))
        .count();
    // Replace entries are transitional helpers that should eventually be replaced
    assert!(
        replace_count >= 1,
        "expected at least 1 Replace entry, got {replace_count}"
    );
}

#[test]
fn surface_contract_disposition_coverage_sums_to_total() {
    let keep = SURFACE_CONTRACT_V1
        .iter()
        .filter(|e| matches!(e.disposition, SurfaceDisposition::Keep))
        .count();
    let replace = SURFACE_CONTRACT_V1
        .iter()
        .filter(|e| matches!(e.disposition, SurfaceDisposition::Replace))
        .count();
    let retire = SURFACE_CONTRACT_V1
        .iter()
        .filter(|e| matches!(e.disposition, SurfaceDisposition::Retire))
        .count();
    assert_eq!(
        keep + replace + retire,
        SURFACE_CONTRACT_V1.len(),
        "all entries should have one of the three dispositions"
    );
}

// =========================================================================
// 9. Source-level migration hygiene (compile-time verification)
// =========================================================================
//
// These tests verify at compile time that certain types and functions are
// accessible through runtime_compat, proving the abstraction layer is
// complete for the core async surface.

#[test]
fn runtime_compat_exports_mutex() {
    // Compile-time: Mutex is accessible through runtime_compat
    let _: runtime_compat::Mutex<u32> = runtime_compat::Mutex::new(0);
}

#[test]
fn runtime_compat_exports_rwlock() {
    let _: runtime_compat::RwLock<u32> = runtime_compat::RwLock::new(0);
}

#[test]
fn runtime_compat_exports_semaphore() {
    let _: runtime_compat::Semaphore = runtime_compat::Semaphore::new(1);
}

#[test]
fn runtime_compat_exports_mpsc_channel() {
    let (_tx, _rx) = runtime_compat::mpsc::channel::<u32>(1);
}

#[test]
fn runtime_compat_exports_watch_channel() {
    let (_tx, _rx) = runtime_compat::watch::channel(0u32);
}

#[test]
fn runtime_compat_exports_oneshot_channel() {
    let (_tx, _rx) = runtime_compat::oneshot::channel::<u32>();
}

#[test]
fn runtime_compat_exports_broadcast_channel() {
    let (_tx, _rx) = runtime_compat::broadcast::channel::<u32>(8);
}

// =========================================================================
// 10. Migration completeness indicators
// =========================================================================

#[test]
fn no_retire_entries_marked_as_keep() {
    // Ensure no entry has been silently re-classified during migration
    for entry in SURFACE_CONTRACT_V1 {
        if entry.api.contains("process::Command") || entry.api.contains("signal") {
            // These are known Retire entries
            let check = matches!(
                entry.disposition,
                SurfaceDisposition::Retire | SurfaceDisposition::Replace
            );
            assert!(
                check,
                "entry '{}' should be Retire or Replace, not Keep",
                entry.api
            );
        }
    }
}

#[test]
fn replace_entries_all_have_explicit_target() {
    for entry in SURFACE_CONTRACT_V1 {
        if matches!(entry.disposition, SurfaceDisposition::Replace) {
            let replacement = entry.replacement.unwrap_or("");
            assert!(
                !replacement.is_empty(),
                "Replace entry '{}' needs an explicit replacement target",
                entry.api
            );
        }
    }
}
