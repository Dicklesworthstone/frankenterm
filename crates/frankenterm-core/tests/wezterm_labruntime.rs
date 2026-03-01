//! LabRuntime port of `#[tokio::test]` async tests from `wezterm.rs`.
//!
//! Each test that previously used `#[tokio::test]` is wrapped in
//! `RuntimeFixture::current_thread()` + `rt.block_on(async { … })`.
//! Feature-gated behind `asupersync-runtime`.
//!
//! Omitted tests:
//! - `retry_with_*` (2): private method `WeztermClient::retry_with`
//! - `waiter_stops_at_max_polls` (1): `#[tokio::test(start_paused = true)]` needs timer control
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use std::sync::Arc;

use common::fixtures::RuntimeFixture;
use frankenterm_core::circuit_breaker::{CircuitBreakerStatus, CircuitStateKind};
use frankenterm_core::error::WeztermError;
use frankenterm_core::wezterm::{
    BackendKind, BackendSelection, MockEvent, MockWezterm, MoveDirection, PaneInfo,
    PaneTextSource, SplitDirection, UnifiedClient, WeztermFuture, WeztermHandle,
    WeztermHandleSource, WeztermInterface,
};

// ===========================================================================
// Local FailingMockWezterm (mirrors #[cfg(test)]-gated version in wezterm.rs)
// ===========================================================================

struct FailingMockWezterm;

impl WeztermInterface for FailingMockWezterm {
    fn list_panes(&self) -> WeztermFuture<'_, Vec<PaneInfo>> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, PaneInfo> {
        Box::pin(async move {
            Err(frankenterm_core::Error::Wezterm(
                WeztermError::PaneNotFound(pane_id),
            ))
        })
    }
    fn get_text(&self, _: u64, _: bool) -> WeztermFuture<'_, String> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn send_text(&self, _: u64, _: &str) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn send_text_no_paste(&self, _: u64, _: &str) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn send_text_with_options(&self, _: u64, _: &str, _: bool, _: bool) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn send_control(&self, _: u64, _: &str) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn send_ctrl_c(&self, _: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn send_ctrl_d(&self, _: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn spawn(&self, _: Option<&str>, _: Option<&str>) -> WeztermFuture<'_, u64> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn activate_pane(&self, _: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn split_pane(
        &self,
        _: u64,
        _: SplitDirection,
        _: Option<&str>,
        _: Option<u8>,
    ) -> WeztermFuture<'_, u64> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn get_pane_direction(&self, _: u64, _: MoveDirection) -> WeztermFuture<'_, Option<u64>> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn kill_pane(&self, _: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn zoom_pane(&self, _: u64, _: bool) -> WeztermFuture<'_, ()> {
        Box::pin(async {
            Err(frankenterm_core::Error::Wezterm(WeztermError::Timeout(5)))
        })
    }
    fn circuit_status(&self) -> CircuitBreakerStatus {
        CircuitBreakerStatus::default()
    }
}

fn mock_wezterm_handle() -> WeztermHandle {
    Arc::new(MockWezterm::new())
}

fn mock_wezterm_handle_failing() -> WeztermHandle {
    Arc::new(FailingMockWezterm)
}

// ===========================================================================
// Section 1: mock_wezterm_handle / mock_wezterm_handle_failing tests
// ===========================================================================

#[test]
fn mock_handle_returns_empty_panes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let handle = mock_wezterm_handle();
        let panes = handle.list_panes().await.unwrap();
        assert!(panes.is_empty());
    });
}

#[test]
fn mock_handle_failing_list_errors() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let handle = mock_wezterm_handle_failing();
        let result = handle.list_panes().await;
        assert!(result.is_err());
    });
}

#[test]
fn mock_handle_failing_get_text_errors() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let handle = mock_wezterm_handle_failing();
        let result = handle.get_text(0, false).await;
        assert!(result.is_err());
    });
}

#[test]
fn mock_handle_failing_send_text_errors() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let handle = mock_wezterm_handle_failing();
        let result = handle.send_text(0, "test").await;
        assert!(result.is_err());
    });
}

#[test]
fn mock_handle_failing_spawn_errors() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let handle = mock_wezterm_handle_failing();
        let result = handle.spawn(None, None).await;
        assert!(result.is_err());
    });
}

#[test]
fn mock_handle_failing_circuit_status_default() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let handle = mock_wezterm_handle_failing();
        let status = handle.circuit_status();
        assert_eq!(status.state, CircuitStateKind::Closed);
    });
}

// ===========================================================================
// Section 2: WeztermHandleSource
// ===========================================================================

#[test]
fn wezterm_handle_source_delegates_get_text() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.inject_output(0, "source test").await.unwrap();

        let handle: WeztermHandle = Arc::new(mock);
        let source = WeztermHandleSource::new(handle);
        let text = source.get_text(0, false).await.unwrap();
        assert_eq!(text, "source test");
    });
}

// ===========================================================================
// Section 3: MockWezterm tests (from mock_tests module)
// ===========================================================================

#[test]
fn mock_add_and_list_panes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.add_default_pane(1).await;

        let panes = mock.list_panes().await.unwrap();
        assert_eq!(panes.len(), 2);
    });
}

#[test]
fn mock_get_text_returns_content() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.inject_output(0, "hello world\n").await.unwrap();

        let text = mock.get_text(0, false).await.unwrap();
        assert_eq!(text, "hello world\n");
    });
}

#[test]
fn mock_send_text_echoes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.send_text(0, "ls -la\n").await.unwrap();

        let text = mock.get_text(0, false).await.unwrap();
        assert_eq!(text, "ls -la\n");
    });
}

#[test]
fn mock_inject_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;

        mock.inject(0, MockEvent::AppendOutput("line 1\n".to_string()))
            .await
            .unwrap();
        mock.inject(0, MockEvent::SetTitle("New Title".to_string()))
            .await
            .unwrap();
        mock.inject(0, MockEvent::Resize(120, 40)).await.unwrap();

        let state = mock.pane_state(0).await.unwrap();
        assert_eq!(state.content, "line 1\n");
        assert_eq!(state.title, "New Title");
        assert_eq!(state.cols, 120);
        assert_eq!(state.rows, 40);
    });
}

#[test]
fn mock_spawn_creates_pane() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        let id = mock.spawn(Some("/tmp"), None).await.unwrap();
        assert_eq!(mock.pane_count().await, 1);

        let pane = mock.get_pane(id).await.unwrap();
        assert_eq!(pane.cwd.as_deref(), Some("/tmp"));
    });
}

#[test]
fn mock_kill_pane_removes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        assert_eq!(mock.pane_count().await, 1);

        mock.kill_pane(0).await.unwrap();
        assert_eq!(mock.pane_count().await, 0);
    });
}

#[test]
fn mock_activate_pane() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.add_default_pane(1).await;

        mock.activate_pane(1).await.unwrap();

        let p0 = mock.pane_state(0).await.unwrap();
        let p1 = mock.pane_state(1).await.unwrap();
        assert!(!p0.is_active);
        assert!(p1.is_active);
    });
}

#[test]
fn mock_zoom_pane() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;

        mock.zoom_pane(0, true).await.unwrap();
        let state = mock.pane_state(0).await.unwrap();
        assert!(state.is_zoomed);

        mock.zoom_pane(0, false).await.unwrap();
        let state = mock.pane_state(0).await.unwrap();
        assert!(!state.is_zoomed);
    });
}

#[test]
fn mock_clear_screen() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.inject_output(0, "some text").await.unwrap();
        mock.inject(0, MockEvent::ClearScreen).await.unwrap();

        let text = mock.get_text(0, false).await.unwrap();
        assert!(text.is_empty());
    });
}

#[test]
fn mock_pane_not_found() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        assert!(mock.get_text(99, false).await.is_err());
        assert!(mock.send_text(99, "x").await.is_err());
        assert!(mock.inject_output(99, "x").await.is_err());
    });
}

#[test]
fn mock_split_pane_creates_new() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;

        let new_id = mock
            .split_pane(0, SplitDirection::Right, None, None)
            .await
            .unwrap();
        assert_eq!(mock.pane_count().await, 2);
        assert_ne!(new_id, 0);
    });
}

#[test]
fn mock_as_wezterm_handle() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.inject_output(0, "test").await.unwrap();

        let handle: WeztermHandle = Arc::new(mock);
        let text = handle.get_text(0, false).await.unwrap();
        assert_eq!(text, "test");
    });
}

#[test]
fn mock_pane_content_isolation() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.add_default_pane(1).await;

        mock.inject_output(0, "pane-zero-only").await.unwrap();
        mock.inject_output(1, "pane-one-only").await.unwrap();

        let t0 = mock.get_text(0, false).await.unwrap();
        let t1 = mock.get_text(1, false).await.unwrap();
        assert!(t0.contains("pane-zero-only"));
        assert!(!t0.contains("pane-one-only"));
        assert!(t1.contains("pane-one-only"));
        assert!(!t1.contains("pane-zero-only"));
    });
}

#[test]
fn mock_pane_size_via_state() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;

        let state = mock.pane_state(0).await.unwrap();
        assert_eq!(state.cols, 80);
        assert_eq!(state.rows, 24);

        mock.inject(0, MockEvent::Resize(200, 50)).await.unwrap();
        let state = mock.pane_state(0).await.unwrap();
        assert_eq!(state.cols, 200);
        assert_eq!(state.rows, 50);
    });
}

#[test]
fn mock_multiple_appends_accumulate() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;

        mock.inject_output(0, "a").await.unwrap();
        mock.inject_output(0, "b").await.unwrap();
        mock.inject_output(0, "c").await.unwrap();

        let text = mock.get_text(0, false).await.unwrap();
        assert_eq!(text, "abc");
    });
}

#[test]
fn mock_spawn_multiple_gets_unique_ids() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        let id1 = mock.spawn(None, None).await.unwrap();
        let id2 = mock.spawn(None, None).await.unwrap();
        let id3 = mock.spawn(None, None).await.unwrap();

        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_eq!(mock.pane_count().await, 3);
    });
}

#[test]
fn mock_kill_nonexistent_pane_is_noop() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        assert!(mock.kill_pane(99).await.is_ok());
    });
}

#[test]
fn mock_split_ignores_parent_creates_new() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        let new_id = mock
            .split_pane(99, SplitDirection::Right, None, None)
            .await
            .unwrap();
        assert_eq!(mock.pane_count().await, 1);
        assert_eq!(new_id, 0);
    });
}

// ===========================================================================
// Section 4: UnifiedClient tests
// ===========================================================================

#[test]
fn unified_client_get_text_delegates() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.inject_output(0, "hello from unified").await.unwrap();

        let handle: WeztermHandle = Arc::new(mock);
        let sel = BackendSelection {
            kind: BackendKind::Cli,
            reason: "test".to_string(),
            compatibility: None,
        };
        let unified = UnifiedClient::from_handle(handle, sel);
        let text = unified.get_text(0, false).await.unwrap();
        assert_eq!(text, "hello from unified");
    });
}

#[test]
fn unified_client_send_text_delegates() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;

        let handle: WeztermHandle = Arc::new(mock);
        let sel = BackendSelection {
            kind: BackendKind::Cli,
            reason: "test".to_string(),
            compatibility: None,
        };
        let unified = UnifiedClient::from_handle(handle, sel);
        unified.send_text(0, "cmd\n").await.unwrap();
        let text = unified.get_text(0, false).await.unwrap();
        assert_eq!(text, "cmd\n");
    });
}

#[test]
fn unified_client_list_panes_delegates() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.add_default_pane(1).await;

        let handle: WeztermHandle = Arc::new(mock);
        let sel = BackendSelection {
            kind: BackendKind::Vendored,
            reason: "test".to_string(),
            compatibility: None,
        };
        let unified = UnifiedClient::from_handle(handle, sel);
        let panes = unified.list_panes().await.unwrap();
        assert_eq!(panes.len(), 2);
    });
}
