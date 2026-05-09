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
//! ## Lifecycle regressions surfaced by this suite
//! ft-dvgzi.1.1 made the client refuse `wezterm cli` autostart fallback
//! when a strict hermetic socket is configured. ft-dvgzi.2.1 then made
//! the fixture's default program persistent, so spawn-created panes stay
//! alive across the follow-up `list_panes` call that proves the mux
//! subprocess did not drop the connection.

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use common::wezterm_subprocess::{WeztermSubprocessFixture, should_run};
#[cfg(all(feature = "vendored", unix))]
use frankenterm_core::vendored::{DirectMuxClient, DirectMuxClientConfig};
use frankenterm_core::wezterm::PaneInfo;
#[cfg(all(feature = "vendored", unix))]
use frankenterm_core::wezterm::SplitDirection;
#[cfg(all(feature = "vendored", unix))]
use frankenterm_term::TerminalSize;

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

#[cfg(all(feature = "vendored", unix))]
fn wait_until_text_contains(
    runtime: &RuntimeFixture,
    client: &frankenterm_core::wezterm::WeztermClient,
    pane_id: u64,
    needle: &str,
    deadline: std::time::Duration,
) -> String {
    let start = std::time::Instant::now();
    loop {
        let client_for_poll = client.clone();
        let text = runtime
            .block_on(Box::pin(client_for_poll.get_text(pane_id, false)))
            .expect("get_text during loopback wait");
        if text.contains(needle) {
            log(
                "loopback",
                "read_observed",
                serde_json::json!({
                    "pane_id": pane_id,
                    "needle": needle,
                    "bytes": text.len(),
                    "elapsed_ms": start.elapsed().as_millis(),
                }),
            );
            return text;
        }
        assert!(
            start.elapsed() < deadline,
            "timed out waiting for pane {pane_id} text to contain {needle:?}; last text={text:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
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
// The fixture uses a persistent default_prog loop so the spawn-created pane
// stays alive long enough for the follow-up listing to observe it.

#[test]
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
// Covers the richer two-pane serde surface after the real mux spawn path has
// observed the persistent default-program pane.

#[test]
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
// The fixture's persistent default_prog keeps fixture A stable across
// spawn+list before the process is dropped and fixture B starts fresh.

#[test]
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
// Polls until the asynchronous spawn-created pane is visible instead of racing
// the renderer/mux update path.

#[test]
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

// ── Test 6: no-mock PTY/mux loopback smoke ──────────────────────────────────
//
// Uses a real mux subprocess, a real PTY-backed `/bin/cat`, the CLI-backed
// WeztermClient for spawn/send/read, and the direct mux client for resize.

#[cfg(all(feature = "vendored", unix))]
#[test]
fn no_mock_spawn_send_resize_read_loopback() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm tests");
        return;
    }
    let fixture = WeztermSubprocessFixture::spawn_with_default_prog(&["/bin/cat"])
        .expect("spawn mux subprocess");
    let client = fixture.client();
    let runtime = RuntimeFixture::current_thread();

    let initial = runtime
        .block_on({
            let client = client.clone();
            async move { client.list_panes().await }
        })
        .expect("initial list_panes");
    assert_eq!(initial.len(), 1, "cat fixture should start with one pane");
    let source = initial[0].clone();
    log(
        "loopback",
        "initial",
        serde_json::json!({
            "pane_id": source.pane_id,
            "tab_id": source.tab_id,
            "window_id": source.window_id,
            "rows": source.effective_rows(),
            "cols": source.effective_cols(),
        }),
    );

    let spawned_pane_id = runtime
        .block_on({
            let client = client.clone();
            async move { client.spawn(None, Some("ft-test")).await }
        })
        .expect("spawn second cat pane");
    let split_pane_id = runtime
        .block_on({
            let client = client.clone();
            async move {
                client
                    .split_pane(source.pane_id, SplitDirection::Right, None, Some(40))
                    .await
            }
        })
        .expect("split source pane");
    let after_spawn = runtime
        .block_on({
            let client = client.clone();
            async move { client.list_panes().await }
        })
        .expect("post-spawn list_panes");
    let ids: Vec<u64> = after_spawn.iter().map(|pane| pane.pane_id).collect();
    log(
        "loopback",
        "spawn_and_split",
        serde_json::json!({
            "spawned_pane_id": spawned_pane_id,
            "split_pane_id": split_pane_id,
            "pane_count": after_spawn.len(),
            "ids": ids,
        }),
    );
    assert!(
        after_spawn
            .iter()
            .any(|pane| pane.pane_id == spawned_pane_id),
        "spawned pane should be listed"
    );
    assert!(
        after_spawn.iter().any(|pane| pane.pane_id == split_pane_id),
        "split pane should be listed"
    );
    assert!(
        after_spawn.len() >= 3,
        "fixture should contain source, spawned, and split panes"
    );

    let mut direct = runtime
        .block_on(async {
            DirectMuxClient::connect(
                DirectMuxClientConfig::default().with_socket_path(fixture.socket_path()),
            )
            .await
        })
        .expect("connect direct mux client");
    let resized = TerminalSize {
        rows: 31,
        cols: 96,
        pixel_width: 960,
        pixel_height: 620,
        dpi: 96,
    };
    runtime
        .block_on(Box::pin(direct.resize(
            source.tab_id,
            source.pane_id,
            resized,
        )))
        .expect("direct mux resize");
    let render = runtime
        .block_on(async { direct.get_pane_render_changes(source.pane_id).await })
        .expect("render changes after resize");
    log(
        "loopback",
        "resized",
        serde_json::json!({
            "pane_id": source.pane_id,
            "rows": render.dimensions.viewport_rows,
            "cols": render.dimensions.cols,
            "pixel_width": render.dimensions.pixel_width,
            "pixel_height": render.dimensions.pixel_height,
        }),
    );
    assert_eq!(render.dimensions.viewport_rows, resized.rows);
    assert_eq!(render.dimensions.cols, resized.cols);

    let token = format!(
        "ft-hme39-loopback-{}-{}",
        source.pane_id,
        std::process::id()
    );
    runtime
        .block_on({
            let client = client.clone();
            let token = token.clone();
            async move {
                client
                    .send_text_with_options(source.pane_id, &format!("{token}\n"), true, true)
                    .await
            }
        })
        .expect("send token to cat pane");
    let text = wait_until_text_contains(
        &runtime,
        &client,
        source.pane_id,
        &token,
        std::time::Duration::from_secs(5),
    );
    assert!(
        text.contains(&token),
        "loopback read should include sent token"
    );
}
