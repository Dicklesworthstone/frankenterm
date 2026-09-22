//! Shared pane-capability evidence resolution for policy-gated actions.
//!
//! Every surface that evaluates a policy decision for pane input (the robot
//! and human CLI, the MCP server, tx prepare gates) must see the same
//! evidence: OSC 133 prompt state reconstructed from captured segments,
//! alt-screen and capture-gap state from the watcher over IPC, and the
//! pane's active reservation. This module is the single implementation
//! (ft-zhwa6); surfaces must not carry private copies, because a copy that
//! drops one input silently weakens its policy gate.

use std::path::Path;

use serde::Deserialize;

use crate::ingest::Osc133State;
use crate::policy::PaneCapabilities;
use crate::storage::StorageHandle;

/// Number of recent segments scanned to reconstruct OSC 133 prompt state.
const OSC_SEGMENT_LIMIT: usize = 200;
/// Column/byte bounds for an error detail embedded in a warning.
const WARNING_DETAIL_MAX_COLUMNS: usize = 400;
const WARNING_DETAIL_MAX_BYTES: usize = 1_600;

/// Watcher-reported pane state, as returned by the IPC `pane_state` call.
#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct IpcPaneState {
    pub pane_id: u64,
    pub known: bool,
    #[serde(default)]
    pub observed: Option<bool>,
    #[serde(default)]
    pub alt_screen: Option<bool>,
    #[serde(default)]
    pub last_status_at: Option<i64>,
    #[serde(default)]
    pub in_gap: Option<bool>,
    #[serde(default)]
    pub cursor_alt_screen: Option<bool>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Resolved capabilities plus the human-readable evidence gaps behind them.
#[derive(Debug, Clone)]
pub struct CapabilityResolution {
    pub capabilities: PaneCapabilities,
    /// Bounded, unsanitized diagnostics; callers that print them must apply
    /// their own terminal sanitization.
    pub warnings: Vec<String>,
}

fn bounded_detail(prefix: &str, detail: &dyn std::fmt::Display) -> String {
    crate::output::truncate_bounded(
        &format!("{prefix}{detail}"),
        WARNING_DETAIL_MAX_COLUMNS,
        WARNING_DETAIL_MAX_BYTES,
    )
}

/// Reconstruct OSC 133 prompt state from the pane's most recent segments.
///
/// # Errors
/// Returns a bounded diagnostic if the segments cannot be read.
pub async fn derive_osc_state_from_storage(
    storage: &StorageHandle,
    pane_id: u64,
) -> Result<Option<Osc133State>, String> {
    let segments = storage
        .get_segments(pane_id, OSC_SEGMENT_LIMIT)
        .await
        .map_err(|error| bounded_detail("failed to read segments: ", &error))?;
    if segments.is_empty() {
        return Ok(None);
    }

    let mut state = Osc133State::new();
    for segment in segments.iter().rev() {
        crate::ingest::process_osc133_output(&mut state, &segment.content);
    }

    if state.markers_seen == 0 {
        return Ok(None);
    }

    Ok(Some(state))
}

/// Ask the watcher for its view of one pane.
///
/// # Errors
/// Returns a bounded diagnostic if the IPC call fails or the payload is
/// malformed.
#[cfg(unix)]
pub async fn fetch_pane_state_from_ipc(
    socket_path: &Path,
    pane_id: u64,
) -> Result<Option<IpcPaneState>, String> {
    #[cfg(test)]
    if let Some(state) = test_pane_state_override(pane_id) {
        return Ok(Some(state));
    }

    let client = crate::ipc::IpcClient::new(socket_path);
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    match client.pane_state_with_cx(&cx, pane_id).await {
        Ok(response) => {
            if !response.ok {
                let detail = response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string());
                return Err(bounded_detail("", &detail));
            }
            if let Some(data) = response.data {
                serde_json::from_value::<IpcPaneState>(data)
                    .map(Some)
                    .map_err(|error| bounded_detail("invalid pane state payload: ", &error))
            } else {
                Ok(None)
            }
        }
        Err(error) => Err(bounded_detail("", &error)),
    }
}

/// Ask the watcher for its view of one pane.
///
/// # Errors
/// Always errors: watcher IPC is Unix-only.
#[cfg(not(unix))]
pub async fn fetch_pane_state_from_ipc(
    _socket_path: &Path,
    _pane_id: u64,
) -> Result<Option<IpcPaneState>, String> {
    #[cfg(test)]
    if let Some(state) = test_pane_state_override(_pane_id) {
        return Ok(Some(state));
    }
    Err("IPC not supported on this platform".to_string())
}

/// Alt-screen state the watcher can vouch for, or `None` when unknown.
#[must_use]
pub fn resolve_alt_screen_state(state: &IpcPaneState) -> Option<bool> {
    if !state.known {
        return None;
    }
    if let Some(cursor_state) = state.cursor_alt_screen {
        return Some(cursor_state);
    }
    if state.last_status_at.is_some() {
        return state.alt_screen;
    }
    None
}

/// Resolve every policy-relevant capability of one pane.
///
/// Missing evidence never widens permissions: unknown prompt state stays
/// inactive, unknown alt-screen stays `None`, and unknown capture continuity
/// is treated as a recent gap. The active reservation is always consulted
/// when storage is available so reservation conflicts are denied on every
/// surface.
pub async fn resolve_pane_capabilities(
    pane_id: u64,
    storage: Option<&StorageHandle>,
    ipc_socket_path: Option<&Path>,
) -> CapabilityResolution {
    let mut warnings = Vec::new();
    let mut osc_state = None;

    if let Some(storage) = storage {
        match derive_osc_state_from_storage(storage, pane_id).await {
            Ok(state) => osc_state = state,
            Err(error) => warnings.push(bounded_detail("OSC 133 state unavailable: ", &error)),
        }
    } else {
        warnings.push("Storage unavailable; prompt state unknown.".to_string());
    }

    let mut alt_screen = None;
    let mut in_gap = true;
    let mut gap_known = false;

    if let Some(socket_path) = ipc_socket_path {
        match fetch_pane_state_from_ipc(socket_path, pane_id).await {
            Ok(Some(state)) => {
                if state.pane_id != pane_id {
                    warnings.push(format!(
                        "Watcher returned state for pane {} (expected {})",
                        state.pane_id, pane_id
                    ));
                }
                if !state.known {
                    let reason = state.reason.as_deref().unwrap_or("unknown");
                    warnings.push(bounded_detail(
                        "",
                        &format_args!("Watcher has no state for this pane ({reason})."),
                    ));
                } else if state.observed == Some(false) {
                    warnings.push(
                        "Pane is not observed by watcher; state may be incomplete.".to_string(),
                    );
                }
                alt_screen = resolve_alt_screen_state(&state);
                if let Some(state_in_gap) = state.in_gap {
                    gap_known = true;
                    in_gap = state_in_gap;
                }
                if alt_screen.is_none() {
                    warnings
                        .push("Alt-screen state unknown; approval may be required.".to_string());
                }
                if in_gap {
                    if gap_known {
                        warnings.push(
                            "Recent capture gap detected; approval may be required.".to_string(),
                        );
                    } else {
                        warnings.push(
                            "Capture continuity unknown; treating as recent gap.".to_string(),
                        );
                    }
                }
            }
            Ok(None) => {
                warnings.push("Watcher IPC returned no pane state.".to_string());
            }
            Err(error) => {
                warnings.push(bounded_detail("Watcher IPC unavailable: ", &error));
            }
        }
    } else {
        warnings.push("IPC socket unavailable; alt-screen/gap unknown.".to_string());
    }

    let mut capabilities =
        PaneCapabilities::from_ingest_state(osc_state.as_ref(), alt_screen, in_gap);

    if let Some(storage) = storage {
        let reservation_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        match storage
            .get_active_reservation_with_cx(&reservation_cx, pane_id)
            .await
        {
            Ok(Some(reservation)) => {
                capabilities.is_reserved = true;
                capabilities.reserved_by = Some(reservation.owner_id);
            }
            Ok(None) => {}
            Err(error) => {
                warnings.push(bounded_detail("Reservation lookup failed: ", &error));
            }
        }
    }

    CapabilityResolution {
        capabilities,
        warnings,
    }
}

#[cfg(test)]
fn test_pane_state_override_slot()
-> &'static std::sync::Mutex<std::collections::HashMap<u64, (IpcPaneState, usize)>> {
    static SLOT: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u64, (IpcPaneState, usize)>>,
    > = std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Keeps a test pane-state override installed while alive.
#[cfg(test)]
pub(crate) struct TestPaneStateOverrideGuard {
    pane_id: u64,
}

#[cfg(test)]
impl Drop for TestPaneStateOverrideGuard {
    fn drop(&mut self) {
        let mut overrides = test_pane_state_override_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Equal overlapping fixtures share custody. One test completing must
        // not remove another test's still-live pane state.
        if let Some((_, owners)) = overrides.get_mut(&self.pane_id) {
            *owners = owners
                .checked_sub(1)
                .expect("fixture guard owns one reference");
            if *owners == 0 {
                overrides.remove(&self.pane_id);
            }
        }
    }
}

/// Make [`fetch_pane_state_from_ipc`] return `state` for its pane while the
/// guard lives, without a running watcher.
#[cfg(test)]
pub(crate) fn set_test_pane_state_override(state: IpcPaneState) -> TestPaneStateOverrideGuard {
    let pane_id = state.pane_id;
    let mut overrides = test_pane_state_override_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match overrides.get_mut(&pane_id) {
        Some((existing, owners)) => {
            assert_eq!(
                existing, &state,
                "conflicting concurrent pane-state fixtures need distinct pane IDs"
            );
            *owners = owners
                .checked_add(1)
                .expect("fixture owner count cannot overflow");
        }
        None => {
            overrides.insert(pane_id, (state, 1));
        }
    }
    TestPaneStateOverrideGuard { pane_id }
}

#[cfg(test)]
pub(crate) fn test_pane_state_override(pane_id: u64) -> Option<IpcPaneState> {
    test_pane_state_override_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&pane_id)
        .map(|(state, _)| state.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_segment_limit_is_bounded() {
        const {
            assert!(OSC_SEGMENT_LIMIT > 0);
            assert!(OSC_SEGMENT_LIMIT <= 1000);
        }
    }

    /// ft-zhwa6: the CLI carried a private resolver copy that never looked up
    /// reservations, so `policy.pane_reserved` could not fire for CLI sends.
    /// Every surface now shares this resolver; pin the reservation input.
    #[test]
    fn reserved_pane_resolves_with_reservation_owner_and_unreserved_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("capabilities.db");
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy())
                .await
                .unwrap();
            for pane_id in [7, 8] {
                storage
                    .upsert_pane(crate::storage::PaneRecord {
                        pane_id,
                        pane_uuid: None,
                        domain: "local".to_string(),
                        window_id: None,
                        tab_id: None,
                        title: None,
                        cwd: None,
                        tty_name: None,
                        first_seen_at: 1_700_000_000_000,
                        last_seen_at: 1_700_000_000_000,
                        observed: true,
                        ignore_reason: None,
                        last_decision_at: None,
                    })
                    .await
                    .unwrap();
            }
            storage
                .create_reservation(7, "workflow", "wf-owner", None, 3_600_000)
                .await
                .unwrap();

            let reserved = resolve_pane_capabilities(7, Some(&storage), None).await;
            assert!(reserved.capabilities.is_reserved);
            assert_eq!(
                reserved.capabilities.reserved_by.as_deref(),
                Some("wf-owner")
            );

            let free = resolve_pane_capabilities(8, Some(&storage), None).await;
            assert!(!free.capabilities.is_reserved);
            assert_eq!(free.capabilities.reserved_by, None);
            assert!(
                free.warnings
                    .iter()
                    .any(|warning| warning.contains("IPC socket unavailable")),
                "missing watcher evidence is reported, not assumed: {:?}",
                free.warnings
            );
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn missing_evidence_never_widens_capabilities() {
        let state = IpcPaneState {
            pane_id: 3,
            known: false,
            observed: None,
            alt_screen: Some(false),
            last_status_at: Some(1),
            in_gap: None,
            cursor_alt_screen: Some(false),
            reason: Some("not tracked".to_string()),
        };
        let _guard = set_test_pane_state_override(state);
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let resolution = runtime.block_on(resolve_pane_capabilities(
            3,
            None,
            Some(Path::new("/nonexistent/ft-capability-test.sock")),
        ));
        assert_eq!(
            resolution.capabilities.alt_screen, None,
            "an unknown watcher state cannot vouch for alt-screen"
        );
        assert!(!resolution.capabilities.is_reserved);
        assert!(
            resolution
                .warnings
                .iter()
                .any(|warning| warning == "Watcher has no state for this pane (not tracked).")
        );
        assert!(
            resolution
                .warnings
                .iter()
                .any(|warning| warning.contains("treating as recent gap"))
        );
    }

    #[test]
    fn equal_pane_overrides_retain_each_live_owner_in_both_drop_orders() {
        let pane_id = 4_299;
        let state = IpcPaneState {
            pane_id,
            known: true,
            observed: Some(true),
            alt_screen: Some(false),
            last_status_at: Some(1_700_000_000_000),
            in_gap: Some(false),
            cursor_alt_screen: Some(false),
            reason: None,
        };
        for drop_older_first in [true, false] {
            let older = set_test_pane_state_override(state.clone());
            let newer = set_test_pane_state_override(state.clone());
            let remaining = if drop_older_first {
                drop(older);
                newer
            } else {
                drop(newer);
                older
            };
            let visible = test_pane_state_override(pane_id)
                .expect("one completing request cannot retire its live peer's fixture");
            assert_eq!(visible, state);
            drop(remaining);
            assert!(test_pane_state_override(pane_id).is_none());
        }
    }
}
