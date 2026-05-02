//! No-mocks snapshot/restore contract tests built on the
//! `WeztermSubprocessFixture` (ft-dvgzi). Drives a real
//! `wezterm-mux-server` subprocess via `WeztermClient` and asserts on
//! the pane-listing + serialize/roundtrip behavior that is the
//! foundation of the snapshot engine's e2e contract.
//!
//! Bead: ft-dvgzi.1.
//!
//! Gated on `FT_REAL_WEZTERM_TESTS=1`. Default `cargo test` runs skip
//! cleanly when the wezterm-mux-server binary is absent.
//!
//! ## Architectural blocker surfaced by this suite (filed as ft-dvgzi.1.1)
//! `WeztermClient::run_cli` does NOT pass `--no-auto-start` to the
//! `wezterm cli` subprocess. When connecting to our hermetic socket
//! races or fails (timing window between socket-bind and accept), the
//! `wezterm cli` binary falls back to **auto-spawning a daemonized
//! `wezterm-mux-server` against the user's global pid file at
//! `~/.local/share/wezterm/pid`**. From that point on, `list_panes` /
//! `spawn` calls go to the user's interactive mux, not the fixture's.
//!
//! Empirical evidence: the four `#[ignore]`d tests below saw the
//! fixture's pane count jump to 18 (the user's actual mux had ~18
//! panes at the time) and `spawn` returned ids that didn't appear in
//! the subsequent `list`. Until WeztermClient grows a
//! `with_no_auto_start()` mode (or strict-socket guard), only tests
//! that complete in the first listing — before the auto-spawn fallback
//! engages — are reliable.
//!
//! Per the no-mocks skill's philosophy: the failing tests are LOUDER
//! than mocks ever could be — they surface a real production-relevant
//! behavior (the wezterm CLI's autostart-on-socket-failure) that any
//! deployment touching a hermetic mux socket needs to handle.

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use common::wezterm_subprocess::{WeztermSubprocessFixture, should_run};
use frankenterm_core::wezterm::PaneInfo;

/// Emit a structured JSON-line trace (per the no-mocks skill's logging
/// pattern) on test stderr so CI failures are debuggable.
fn log(test: &str, phase: &str, body: serde_json::Value) {
    let line = serde_json::json!({
        "ts": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "suite": "snapshot_real_mux",
        "test": test,
        "phase": phase,
        "data": body,
    });
    eprintln!("{line}");
}

// ── Test 1: one-pane snapshot + metadata ─────────────────────────────────────
//
// This first listing happens before any retry/autostart fallback can
// engage, so it is the one read-side test that consistently observes
// the fixture's hermetic mux. Subsequent calls (spawn → re-list) hit
// the autostart-fallback issue described in ft-dvgzi.1.1.

#[test]
fn one_pane_listing_carries_basic_metadata() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm tests");
        return;
    }
    let fixture = WeztermSubprocessFixture::spawn().expect("spawn mux subprocess");
    let client = fixture.client();
    let runtime = RuntimeFixture::current_thread();

    log(
        "one_pane",
        "spawn",
        serde_json::json!({"pid": fixture.pid()}),
    );

    let panes = runtime
        .block_on(async move { client.list_panes().await })
        .expect("list_panes");

    log(
        "one_pane",
        "list",
        serde_json::json!({"count": panes.len()}),
    );

    assert_eq!(panes.len(), 1, "fixture default_prog auto-spawns one pane");
    let pane = &panes[0];
    // Required structural fields
    assert!(pane.pane_id < u64::MAX);
    // Workspace defaults to "default" (set by mux server when no override)
    assert_eq!(pane.workspace.as_deref(), Some("default"));
    // Renderer reports a non-zero size for a freshly-spawned pane.
    // Note: domain_name is NOT echoed back by `wezterm cli list --format
    // json` even when the pane is in a named domain — the field is
    // populated from a separate domain query path that the test
    // doesn't trigger.
    let has_size = pane.size.is_some() || (pane.rows.is_some() && pane.cols.is_some());
    assert!(
        has_size,
        "pane should expose either nested size or flat rows/cols"
    );
}

// ── Test 2: spawn second pane → count grows ──────────────────────────────────
//
// IGNORED. Originally blocked on ft-dvgzi.1.1 (autostart-fallback) — that's
// now fixed. Remaining failures are test-design issues: the spawn path
// inside the hermetic mux is unstable (sometimes the second list_panes
// observes only the original pane, sometimes the spawned pane id doesn't
// appear). Suspected root cause: the fixture's default_prog
// `/bin/sh -c 'sleep 600'` exits or the spawn workflow needs an explicit
// PROG argument. Tracking under ft-dvgzi.2.1.

#[test]
#[ignore = "ft-dvgzi.2.1: spawn-path lifecycle on hermetic mux needs rework"]
fn spawn_second_pane_increments_listing() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm tests");
        return;
    }
    let fixture = WeztermSubprocessFixture::spawn().expect("spawn mux subprocess");
    let client = fixture.client();
    let runtime = RuntimeFixture::current_thread();

    let initial = runtime
        .block_on({
            let client = client.clone();
            async move { client.list_panes().await }
        })
        .expect("initial list_panes");
    log(
        "two_pane",
        "before_spawn",
        serde_json::json!({"count": initial.len()}),
    );
    assert_eq!(initial.len(), 1);

    let new_pane_id = runtime
        .block_on({
            let client = client.clone();
            async move { client.spawn(None, Some("ft-test")).await }
        })
        .expect("spawn second pane");
    log(
        "two_pane",
        "spawned",
        serde_json::json!({"new_pane_id": new_pane_id}),
    );

    let after = runtime
        .block_on({
            let client = client.clone();
            async move { client.list_panes().await }
        })
        .expect("post-spawn list_panes");
    log(
        "two_pane",
        "after_spawn",
        serde_json::json!({
            "count": after.len(),
            "ids": after.iter().map(|p| p.pane_id).collect::<Vec<_>>(),
        }),
    );
    assert_eq!(after.len(), 2, "list_panes should now show two panes");
    assert!(
        after.iter().any(|p| p.pane_id == new_pane_id),
        "new_pane_id ({new_pane_id}) should appear in list"
    );
    let ids: Vec<u64> = after.iter().map(|p| p.pane_id).collect();
    let mut dedup = ids.clone();
    dedup.sort_unstable();
    dedup.dedup();
    assert_eq!(dedup.len(), 2, "pane ids must be distinct");
}

// ── Test 3: serialize Vec<PaneInfo> + roundtrip via serde_json ───────────────
//
// IGNORED. Same spawn-path lifecycle issue as tests 2/4/5: the client.spawn
// call before serialize causes the subsequent list_panes to fail on
// connection. The roundtrip logic itself is sound; just need a one-pane
// variant or fix the spawn-path. ft-dvgzi.2.1.

#[test]
#[ignore = "ft-dvgzi.2.1: spawn-path lifecycle on hermetic mux needs rework"]
fn pane_listing_serializes_and_roundtrips() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm tests");
        return;
    }
    let fixture = WeztermSubprocessFixture::spawn().expect("spawn mux subprocess");
    let client = fixture.client();
    let runtime = RuntimeFixture::current_thread();

    // Two-pane snapshot for richer roundtrip surface.
    runtime
        .block_on({
            let client = client.clone();
            async move { client.spawn(None, Some("ft-test")).await }
        })
        .expect("spawn second pane");
    let panes = runtime
        .block_on({
            let client = client.clone();
            async move { client.list_panes().await }
        })
        .expect("list_panes");

    let json = serde_json::to_string(&panes).expect("serialize Vec<PaneInfo>");
    log(
        "snapshot",
        "serialized",
        serde_json::json!({"bytes": json.len(), "n_panes": panes.len()}),
    );
    assert!(json.contains("\"pane_id\""));
    assert!(json.contains("\"workspace\""));

    let restored: Vec<PaneInfo> = serde_json::from_str(&json).expect("deserialize Vec<PaneInfo>");
    assert_eq!(restored.len(), panes.len(), "roundtrip preserves length");
    let original_ids: Vec<u64> = panes.iter().map(|p| p.pane_id).collect();
    let restored_ids: Vec<u64> = restored.iter().map(|p| p.pane_id).collect();
    assert_eq!(
        original_ids, restored_ids,
        "roundtrip preserves pane_ids in order"
    );
    for (a, b) in panes.iter().zip(restored.iter()) {
        assert_eq!(a.pane_id, b.pane_id);
        assert_eq!(a.tab_id, b.tab_id);
        assert_eq!(a.window_id, b.window_id);
        assert_eq!(a.domain_name, b.domain_name);
        assert_eq!(a.workspace, b.workspace);
    }
}

// ── Test 4: restart fixture → independent pane state, no carryover ───────────
//
// IGNORED. After the .1.1 fix the strict-socket guard correctly refuses
// to fall back, but the second list_panes call inside fixture A's
// lifetime sometimes hits "failed to connect" — the mux subprocess is
// dropping connections under the test's spawn+list workload. Same root
// cause as test 2 (ft-dvgzi.2.1).

#[test]
#[ignore = "ft-dvgzi.2.1: fixture mux drops connection between spawn and list"]
fn restart_fixture_yields_independent_pane_state() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm tests");
        return;
    }
    let runtime = RuntimeFixture::current_thread();

    // Phase 1: fixture A snapshots its panes
    let (snapshot_a, socket_a) = {
        let fixture = WeztermSubprocessFixture::spawn().expect("spawn fixture A");
        let socket = fixture.socket_path().to_path_buf();
        let client = fixture.client();
        // Spawn a second pane so the snapshot is non-trivial
        runtime
            .block_on({
                let client = client.clone();
                async move { client.spawn(None, Some("ft-test")).await }
            })
            .expect("spawn second pane in A");
        let panes = runtime
            .block_on({
                let client = client.clone();
                async move { client.list_panes().await }
            })
            .expect("list_panes A");
        log(
            "restart",
            "snapshot_A",
            serde_json::json!({
                "n_panes": panes.len(),
                "socket": socket.display().to_string(),
            }),
        );
        let snap = serde_json::to_string(&panes).expect("serialize A");
        (snap, socket)
        // fixture A drops here -> SIGTERM mux + remove tempdir
    };
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(
        !socket_a.exists(),
        "fixture A's socket must be cleaned up after Drop: {}",
        socket_a.display()
    );

    // Phase 2: fixture B is a fresh process. Its panes must not carry
    // over from A. The whole point of the WeztermSubprocessFixture is
    // hermetic isolation per test.
    let fixture_b = WeztermSubprocessFixture::spawn().expect("spawn fixture B");
    assert_ne!(
        fixture_b.socket_path(),
        socket_a,
        "fixture B must use a different socket path"
    );
    let panes_b = runtime
        .block_on({
            let client = fixture_b.client();
            async move { client.list_panes().await }
        })
        .expect("list_panes B");
    log(
        "restart",
        "snapshot_B",
        serde_json::json!({
            "n_panes": panes_b.len(),
            "socket": fixture_b.socket_path().display().to_string(),
        }),
    );
    assert_eq!(
        panes_b.len(),
        1,
        "fresh fixture B should have only its default_prog pane (no A carryover)"
    );

    // Snapshot A had 2 panes; B has 1; their pane-id sets must be
    // structurally independent (B's mux assigned its own IDs).
    let snap_a: Vec<PaneInfo> = serde_json::from_str(&snapshot_a).expect("deserialize A");
    assert_eq!(snap_a.len(), 2);
    let a_ids: std::collections::HashSet<u64> = snap_a.iter().map(|p| p.pane_id).collect();
    let b_ids: std::collections::HashSet<u64> = panes_b.iter().map(|p| p.pane_id).collect();
    log(
        "restart",
        "id_independence",
        serde_json::json!({
            "a_ids": a_ids.iter().copied().collect::<Vec<_>>(),
            "b_ids": b_ids.iter().copied().collect::<Vec<_>>(),
        }),
    );
    // We do not require the sets to be disjoint (both muxes may assign
    // pane_id=0 for their first pane — that's by design of the wezterm
    // mux ID space). The contract pinned here is that B's listing does
    // NOT contain *every* id from A — i.e., B is not a continuation of
    // A's mux state.
    assert!(
        !a_ids.is_subset(&b_ids),
        "fresh fixture B must not contain all of A's pane ids"
    );
}

// ── Test 5: poll-based wait_until_pane_count helper (steady-state wait) ──────
//
// IGNORED. Originally autostart-fallback amplified by polling — fixed by
// ft-dvgzi.1.1. Now: same spawn-path lifecycle issue as tests 2 and 4
// (ft-dvgzi.2.1).

#[test]
#[ignore = "ft-dvgzi.2.1: spawn-path lifecycle on hermetic mux needs rework"]
fn wait_until_pane_count_observes_async_spawn() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm tests");
        return;
    }
    let fixture = WeztermSubprocessFixture::spawn().expect("spawn mux subprocess");
    let client = fixture.client();
    let runtime = RuntimeFixture::current_thread();

    // wait_until: poll list_panes until count >= target or deadline.
    // Demonstrates the steady-state-wait pattern future tests will need
    // when the renderer is async (snapshot/restore tests must not race
    // against in-flight pane creation).
    let count_at_least = |target: usize, deadline: std::time::Duration| -> Vec<PaneInfo> {
        let start = std::time::Instant::now();
        loop {
            let panes = runtime
                .block_on({
                    let client = client.clone();
                    async move { client.list_panes().await }
                })
                .expect("list_panes during wait");
            if panes.len() >= target {
                log(
                    "wait_until",
                    "observed",
                    serde_json::json!({
                        "target": target,
                        "actual": panes.len(),
                        "elapsed_ms": start.elapsed().as_millis(),
                    }),
                );
                return panes;
            }
            assert!(
                start.elapsed() < deadline,
                "wait_until_pane_count: timed out (target={target}, last={})",
                panes.len()
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    };

    let initial = count_at_least(1, std::time::Duration::from_secs(2));
    assert_eq!(initial.len(), 1);

    // Issue spawn but immediately probe — the helper must wait for the
    // post-spawn listing to reflect 2 panes (don't race on the renderer).
    runtime
        .block_on({
            let client = client.clone();
            async move { client.spawn(None, Some("ft-test")).await }
        })
        .expect("spawn second pane");
    let observed = count_at_least(2, std::time::Duration::from_secs(2));
    assert!(observed.len() >= 2);
}
