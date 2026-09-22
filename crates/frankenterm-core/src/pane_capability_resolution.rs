//! Shared pane-capability evidence resolution for policy-gated actions.
//!
//! Every surface that evaluates a policy decision for pane input (the robot
//! and human CLI, the MCP server, tx prepare gates) must see the same
//! evidence: OSC 133 prompt state reconstructed from captured segments,
//! alt-screen and capture-gap state from the live watcher registry or IPC, and the
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

/// Concrete watcher authority, either remote IPC or the observation runtime's
/// live registry. Both feed the same policy evidence normalization below.
#[derive(Clone)]
pub enum WatcherCapabilitySource {
    Ipc(std::path::PathBuf),
    Registry(std::sync::Arc<crate::runtime_async::RwLock<crate::ingest::PaneRegistry>>),
}

impl WatcherCapabilitySource {
    async fn pane_state(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
    ) -> Result<Option<IpcPaneState>, String> {
        match self {
            Self::Ipc(path) => fetch_pane_state_from_ipc(cx, path, pane_id).await,
            Self::Registry(registry) => {
                let registry = registry
                    .read_with_cx(cx)
                    .await
                    .map_err(|error| bounded_detail("watcher registry unavailable: ", &error))?;
                let Some(entry) = registry.get_entry(pane_id) else {
                    return Ok(None);
                };
                let cursor = registry.get_cursor(pane_id);
                Ok(Some(IpcPaneState {
                    pane_id,
                    known: true,
                    observed: Some(entry.should_observe()),
                    alt_screen: None,
                    last_status_at: None,
                    in_gap: cursor.map(|cursor| cursor.in_gap),
                    cursor_alt_screen: cursor.map(|cursor| cursor.in_alt_screen),
                    reason: None,
                }))
            }
        }
    }
}

struct BoundedWarning {
    text: String,
    overflowed: bool,
}

impl std::fmt::Write for BoundedWarning {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        if self.overflowed
            || !self
                .text
                .len()
                .checked_add(value.len())
                .is_some_and(|length| length <= WARNING_DETAIL_MAX_BYTES)
        {
            self.overflowed = true;
            return Err(std::fmt::Error);
        }
        self.text.push_str(value);
        Ok(())
    }
}

fn bounded_detail(prefix: &str, detail: &dyn std::fmt::Display) -> String {
    let mut buffer = BoundedWarning {
        text: String::with_capacity(WARNING_DETAIL_MAX_BYTES),
        overflowed: false,
    };
    if std::fmt::write(&mut buffer, format_args!("{prefix}{detail}")).is_err() || buffer.overflowed
    {
        return "diagnostic unavailable".to_string();
    }
    crate::output::truncate_bounded(
        &buffer.text,
        WARNING_DETAIL_MAX_COLUMNS,
        WARNING_DETAIL_MAX_BYTES,
    )
}

/// Reconstruct OSC 133 prompt state from the pane's most recent segments.
///
/// # Errors
/// Returns a bounded diagnostic if the segments cannot be read.
pub async fn derive_osc_state_from_storage(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    pane_id: u64,
) -> Result<Option<Osc133State>, String> {
    let segments = storage
        .get_segments_with_cx(cx, pane_id, OSC_SEGMENT_LIMIT)
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
    cx: &crate::cx::Cx,
    socket_path: &Path,
    pane_id: u64,
) -> Result<Option<IpcPaneState>, String> {
    cx.checkpoint()
        .map_err(|error| bounded_detail("pane state request cancelled: ", &error))?;
    #[cfg(test)]
    if let Some(state) = test_pane_state_override(pane_id) {
        return Ok(Some(state));
    }

    let client = crate::ipc::IpcClient::new(socket_path);
    match client.pane_state_with_cx(cx, pane_id).await {
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
    cx: &crate::cx::Cx,
    _socket_path: &Path,
    _pane_id: u64,
) -> Result<Option<IpcPaneState>, String> {
    cx.checkpoint()
        .map_err(|error| bounded_detail("pane state request cancelled: ", &error))?;
    #[cfg(test)]
    if let Some(state) = test_pane_state_override(_pane_id) {
        return Ok(Some(state));
    }
    Err("IPC not supported on this platform".to_string())
}

/// Alt-screen state the watcher can vouch for, or `None` when unknown.
#[must_use]
pub fn resolve_alt_screen_state(state: &IpcPaneState) -> Option<bool> {
    if !state.known || state.observed == Some(false) {
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
/// surface. Missing or failed reservation lookups retain unknown authority;
/// only a successful empty lookup establishes that the pane is unreserved.
pub async fn resolve_pane_capabilities(
    cx: &crate::cx::Cx,
    pane_id: u64,
    storage: Option<&StorageHandle>,
    ipc_socket_path: Option<&Path>,
) -> CapabilityResolution {
    let source = ipc_socket_path.map(|path| WatcherCapabilitySource::Ipc(path.to_path_buf()));
    resolve_pane_capabilities_with_source(cx, pane_id, storage, source.as_ref()).await
}

/// Resolve capabilities from the actual watcher source without assuming shell
/// state when that source is absent, unavailable, or does not know this pane.
pub async fn resolve_pane_capabilities_with_source(
    cx: &crate::cx::Cx,
    pane_id: u64,
    storage: Option<&StorageHandle>,
    source: Option<&WatcherCapabilitySource>,
) -> CapabilityResolution {
    let mut warnings = Vec::new();
    let mut osc_state = None;

    if let Some(storage) = storage {
        match derive_osc_state_from_storage(cx, storage, pane_id).await {
            Ok(state) => osc_state = state,
            Err(error) => warnings.push(bounded_detail("OSC 133 state unavailable: ", &error)),
        }
    } else {
        warnings.push("Storage unavailable; prompt state unknown.".to_string());
    }

    let mut alt_screen = None;
    let mut in_gap = true;
    let mut gap_known = false;

    if let Some(source) = source {
        match source.pane_state(cx, pane_id).await {
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
                if state.pane_id == pane_id && state.known && state.observed != Some(false) {
                    alt_screen = resolve_alt_screen_state(&state);
                    if let Some(state_in_gap) = state.in_gap {
                        gap_known = true;
                        in_gap = state_in_gap;
                    }
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
                warnings.push("Watcher returned no pane state.".to_string());
            }
            Err(error) => {
                warnings.push(bounded_detail("Watcher unavailable: ", &error));
            }
        }
    } else {
        warnings.push("Watcher source unavailable; alt-screen/gap unknown.".to_string());
    }

    let mut capabilities =
        PaneCapabilities::from_ingest_state(osc_state.as_ref(), alt_screen, in_gap);

    if let Some(storage) = storage {
        match storage.get_active_reservation_with_cx(cx, pane_id).await {
            Ok(Some(reservation)) => {
                capabilities.is_reserved = Some(true);
                capabilities.reserved_by = Some(reservation.owner_id);
            }
            Ok(None) => capabilities.is_reserved = Some(false),
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
    use crate::runtime_async::CompatRuntime;

    #[test]
    fn warning_formatting_stops_at_the_byte_budget() {
        struct StreamingDetail(std::cell::Cell<usize>);
        impl std::fmt::Display for StreamingDetail {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                for _ in 0..1_000_000 {
                    self.0.set(self.0.get() + 1);
                    formatter.write_str(
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    )?;
                }
                Ok(())
            }
        }
        let detail = StreamingDetail(std::cell::Cell::new(0));
        assert_eq!(bounded_detail("", &detail), "diagnostic unavailable");
        assert_eq!(detail.0.get(), WARNING_DETAIL_MAX_BYTES / 64 + 1);
        assert_eq!(bounded_detail("Error: ", &"échec"), "Error: échec");
    }

    #[test]
    fn foreign_unknown_and_unobserved_watcher_states_cannot_clear_a_gap() {
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        for (pane_id, returned_id, known, observed) in [
            (4_301, 4_302, true, Some(true)),
            (4_303, 4_303, false, Some(true)),
            (4_304, 4_304, true, Some(false)),
        ] {
            let state = IpcPaneState {
                pane_id: returned_id,
                known,
                observed,
                alt_screen: Some(false),
                last_status_at: Some(1),
                in_gap: Some(false),
                cursor_alt_screen: Some(false),
                reason: None,
            };
            assert!(
                test_pane_state_override_slot()
                    .lock()
                    .unwrap()
                    .insert(pane_id, (state, 1))
                    .is_none()
            );
            let _guard = TestPaneStateOverrideGuard { pane_id };
            let resolution = runtime.block_on(resolve_pane_capabilities(
                &crate::cx::for_testing(),
                pane_id,
                None,
                Some(Path::new("/nonexistent/ft-capability-test.sock")),
            ));
            assert_eq!(resolution.capabilities.alt_screen, None);
            assert!(resolution.capabilities.has_recent_gap);
            assert!(resolution.warnings.iter().any(|warning| {
                warning == "Capture continuity unknown; treating as recent gap."
            }));
        }
    }

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

            let cx = crate::cx::for_testing();
            let reserved = resolve_pane_capabilities(&cx, 7, Some(&storage), None).await;
            assert_eq!(reserved.capabilities.is_reserved, Some(true));
            assert_eq!(
                reserved.capabilities.reserved_by.as_deref(),
                Some("wf-owner")
            );

            let free = resolve_pane_capabilities(&cx, 8, Some(&storage), None).await;
            assert_eq!(free.capabilities.is_reserved, Some(false));
            assert_eq!(free.capabilities.reserved_by, None);
            assert_eq!(free.capabilities.alt_screen, None);
            assert!(free.capabilities.has_recent_gap);
            assert!(!free.capabilities.prompt_active);
            assert!(
                free.warnings
                    .iter()
                    .any(|warning| warning.contains("Watcher source unavailable")),
                "missing watcher evidence is reported, not assumed: {:?}",
                free.warnings
            );

            let mut policy = crate::policy::PolicyEngine::permissive();
            let free_input = crate::policy::PolicyInput::new(
                crate::policy::ActionKind::SendText,
                crate::policy::ActorKind::Human,
            )
            .with_pane(8)
            .with_capabilities(free.capabilities);
            assert!(policy.authorize(&free_input).is_allowed());

            // A cancelled caller cannot inherit a successful empty reservation
            // lookup from a fresh context manufactured inside the resolver.
            let cancelled = crate::cx::for_testing();
            cancelled.cancel_with(
                crate::outcome::CancelKind::User,
                Some("resolver caller cancelled"),
            );
            let cancelled_resolution =
                resolve_pane_capabilities(&cancelled, 8, Some(&storage), None).await;
            assert_eq!(cancelled_resolution.capabilities.is_reserved, None);
            assert_eq!(cancelled_resolution.capabilities.reserved_by, None);
            assert!(
                cancelled_resolution
                    .warnings
                    .iter()
                    .any(|warning| warning.starts_with("Reservation lookup failed:"))
            );

            // A corrupt expiry field must not turn an existing reservation into
            // an apparently free pane. Exercise the actual database decoder,
            // resolver, and authorization path rather than fabricating an error.
            let connection = rusqlite::Connection::open(&db_path).unwrap();
            assert_eq!(
                connection
                    .execute(
                        "UPDATE pane_reservations SET expires_at = 'invalid-expiry' WHERE pane_id = 7",
                        [],
                    )
                    .unwrap(),
                1
            );
            assert!(storage.get_active_reservation(7).await.is_err());
            let unreadable = resolve_pane_capabilities(&cx, 7, Some(&storage), None).await;
            assert_eq!(unreadable.capabilities.is_reserved, None);
            assert!(
                unreadable
                    .warnings
                    .iter()
                    .any(|warning| warning.starts_with("Reservation lookup failed:"))
            );
            let unreadable_input = crate::policy::PolicyInput::new(
                crate::policy::ActionKind::SendText,
                crate::policy::ActorKind::Human,
            )
            .with_pane(7)
            .with_capabilities(unreadable.capabilities);
            assert_eq!(
                policy.authorize(&unreadable_input).rule_id(),
                Some("policy.reservation_unknown")
            );
            storage.shutdown().await.unwrap();
        });
    }

    /// ft-zhwa6's original symptom: CLI tx prepare gates evaluated every pane
    /// as `PaneCapabilities::unknown`, so a `PromptActive` precondition could
    /// never pass. The captured OSC 133 prompt marker is that evidence.
    #[test]
    fn captured_prompt_marker_yields_prompt_active_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("prompt-evidence.db");
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy())
                .await
                .unwrap();
            for pane_id in [21, 22] {
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
                .append_segment(21, "\u{1b}]133;A\u{7}$ ", None)
                .await
                .unwrap();
            storage
                .append_segment(22, "plain output without shell integration\n", None)
                .await
                .unwrap();

            let cx = crate::cx::for_testing();
            let prompt = resolve_pane_capabilities(&cx, 21, Some(&storage), None).await;
            assert!(prompt.capabilities.prompt_active);
            assert!(!prompt.capabilities.command_running);

            let unmarked = resolve_pane_capabilities(&cx, 22, Some(&storage), None).await;
            assert!(
                !unmarked.capabilities.prompt_active,
                "output without OSC 133 markers is not prompt evidence"
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
            &crate::cx::for_testing(),
            3,
            None,
            Some(Path::new("/nonexistent/ft-capability-test.sock")),
        ));
        assert_eq!(
            resolution.capabilities.alt_screen, None,
            "an unknown watcher state cannot vouch for alt-screen"
        );
        assert_eq!(resolution.capabilities.is_reserved, None);
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
