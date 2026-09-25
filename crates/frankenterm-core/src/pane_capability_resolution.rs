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

use serde::{Deserialize, Serialize};

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
    /// Prompt state the watcher read from the live mux's semantic zones.
    #[serde(default)]
    pub live_prompt: Option<LivePromptEvidence>,
}

/// Shell state reconstructed from the live mux's OSC 133 semantic zones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveShellState {
    /// The pane has no prompt or input zones: no shell integration.
    NoIntegration,
    /// The cursor sits on a prompt zone.
    Prompt,
    /// The cursor sits on a command-input zone.
    Input,
    /// Output follows the last prompt, or the cursor left the prompt line.
    CommandRunning,
    /// The semantic query failed; `detail` says why.
    Unavailable,
    /// A known agent TUI (Codex, Claude Code) shows its empty-handed input
    /// composer: no work in progress and no pending decision (ft-t4sma).
    AgentReady,
}

/// What a known agent TUI's screen shows, read from the live screen tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentScreenState {
    /// Input composer visible, nothing running, no menu or confirmation.
    Ready,
    /// The agent is working (it offers to interrupt).
    Busy,
    /// A menu, confirmation or trust prompt is waiting for a human choice.
    /// Typed text plus Enter would select an option, so never type here.
    AwaitingDecision,
}

/// Rows of the screen tail the agent classifier looks at.
const AGENT_SCREEN_TAIL_ROWS: usize = 40;

/// Classify the bottom of a pane's screen as a known agent TUI state.
///
/// Conservative by construction: a decision prompt anywhere in the tail wins,
/// then any busy marker, and only an exact agent composer layout reads as
/// ready. Unrecognized screens return `None` and keep their other evidence.
#[must_use]
pub fn classify_agent_screen(tail: &str) -> Option<(&'static str, AgentScreenState)> {
    let lines: Vec<&str> = tail
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let lines = &lines[lines.len().saturating_sub(AGENT_SCREEN_TAIL_ROWS)..];
    let lower: Vec<String> = lines.iter().map(|line| line.to_lowercase()).collect();
    let any = |needles: &[&str]| {
        lower
            .iter()
            .any(|line| needles.iter().any(|needle| line.contains(needle)))
    };
    // Box-drawing borders are stripped so boxed menus read like plain ones.
    let content = |line: &str| {
        line.trim_matches(|c: char| c == '│' || c.is_whitespace())
            .to_string()
    };
    let selected_menu_item = lines.iter().any(|line| {
        let text = content(*line);
        ['›', '❯', '●', '>'].iter().any(|marker| {
            text.strip_prefix(*marker)
                .map(str::trim_start)
                .is_some_and(|rest| {
                    rest.split_once('.')
                        .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                })
        })
    });

    let codex_composer = lines.iter().any(|line| line.starts_with("› "))
        && any(&["? for shortcuts"]);
    let claude_composer = lines.windows(3).any(|window| {
        window[0].trim_start().starts_with('─')
            && window[1].starts_with('❯')
            && window[2].trim_start().starts_with('─')
    });
    let agent = if codex_composer || any(&["openai codex", "codex can read", "agent command center"]) {
        "codex"
    } else if claude_composer || any(&["claude code"]) {
        "claude_code"
    } else if any(&["gemini cli", "gemini code assist"]) {
        "gemini"
    } else {
        return None;
    };

    if selected_menu_item
        || any(&[
            "enter to confirm",
            "enter to select",
            "enter continue",
            "esc to cancel",
            "trust this folder",
            "trust the files",
            "do you want to",
            "(y/n)",
            "[y/n]",
            "allow command",
            "approve",
        ])
    {
        return Some((agent, AgentScreenState::AwaitingDecision));
    }
    if any(&["esc to interrupt", "to interrupt)", "ctrl+c to interrupt"]) {
        return Some((agent, AgentScreenState::Busy));
    }
    if codex_composer || claude_composer {
        return Some((agent, AgentScreenState::Ready));
    }
    None
}

/// Live prompt evidence carried in the watcher's `pane_state` reply.
///
/// Vendored capture stores rendered rows, so OSC 133 bytes never reach
/// storage: the mux consumes them into per-cell semantic types. This is the
/// evidence that survives, read on demand from the mux.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LivePromptEvidence {
    pub shell_state: LiveShellState,
    #[serde(default)]
    pub last_exit_code: Option<i32>,
    #[serde(default)]
    pub detail: Option<String>,
}

impl LivePromptEvidence {
    /// Evidence for a failed semantic query.
    #[must_use]
    pub fn unavailable(detail: &dyn std::fmt::Display) -> Self {
        Self {
            shell_state: LiveShellState::Unavailable,
            last_exit_code: None,
            detail: Some(bounded_detail("", detail)),
        }
    }

    /// Classify a semantic-zone snapshot. `cursor_row` is the cursor's stable
    /// row, in the same coordinates as the zones.
    #[must_use]
    pub fn from_semantic_zones(
        snapshot: &crate::wezterm::MuxSemanticSnapshot,
        cursor_row: Option<i64>,
    ) -> Self {
        use crate::wezterm::MuxSemanticZoneKind as Kind;
        let integrated = snapshot
            .zones
            .iter()
            .any(|zone| matches!(zone.semantic_type, Kind::Prompt | Kind::Input));
        // Unmarked cells default to Output, so blank Output padding says
        // nothing about the shell.
        let last = snapshot
            .zones
            .iter()
            .filter(|zone| zone.semantic_type != Kind::Output || !zone.text.trim().is_empty())
            .max_by_key(|zone| (zone.end_y, zone.end_x));
        let shell_state = match last {
            _ if !integrated => LiveShellState::NoIntegration,
            None => LiveShellState::NoIntegration,
            Some(zone) => {
                // A submitted command with no output yet leaves the last zone
                // an input zone, but moves the cursor below it.
                let cursor_left_zone = cursor_row
                    .and_then(|row| isize::try_from(row).ok())
                    .is_some_and(|row| row > zone.end_y);
                match zone.semantic_type {
                    Kind::Output => LiveShellState::CommandRunning,
                    _ if cursor_left_zone => LiveShellState::CommandRunning,
                    Kind::Prompt => LiveShellState::Prompt,
                    Kind::Input => LiveShellState::Input,
                }
            }
        };
        Self {
            shell_state,
            last_exit_code: snapshot.last_exit_code,
            detail: None,
        }
    }

    /// The equivalent OSC 133 tracker state, or `None` without integration.
    #[must_use]
    pub fn osc_state(&self) -> Option<Osc133State> {
        let state = match self.shell_state {
            LiveShellState::NoIntegration | LiveShellState::Unavailable => return None,
            LiveShellState::Prompt | LiveShellState::AgentReady => {
                crate::ingest::ShellState::PromptActive
            }
            LiveShellState::Input => crate::ingest::ShellState::InputActive,
            LiveShellState::CommandRunning => crate::ingest::ShellState::CommandRunning,
        };
        let mut osc = Osc133State::new();
        osc.state = state;
        osc.last_exit_code = self.last_exit_code;
        osc.markers_seen = 1;
        Some(osc)
    }
}

/// Read live prompt evidence for one pane from the mux, bounded by `budget`.
pub async fn fetch_live_prompt_evidence(
    cx: &crate::cx::Cx,
    mux: &crate::wezterm::WeztermHandle,
    pane_id: u64,
    budget: std::time::Duration,
) -> LivePromptEvidence {
    let fetch = async {
        let snapshot = match mux.get_semantic_zones_with_cx(cx, pane_id).await {
            Ok(snapshot) => snapshot,
            Err(error) => return LivePromptEvidence::unavailable(&error),
        };
        // Without a cursor row a submitted-but-silent command reads as input;
        // the zone kind alone still separates prompt from running output.
        let cursor_row = mux.list_panes_with_cx(cx).await.ok().and_then(|panes| {
            panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .and_then(|pane| pane.cursor_y)
                .map(i64::from)
        });
        let evidence = LivePromptEvidence::from_semantic_zones(&snapshot, cursor_row);
        // Agent TUIs publish no OSC 133 of their own, and inside an integrated
        // shell they read as a running command. Their screen says more.
        if !matches!(
            evidence.shell_state,
            LiveShellState::NoIntegration | LiveShellState::CommandRunning
        ) {
            return evidence;
        }
        let Ok(tail) = mux
            .get_text_tail_with_cx(cx, pane_id, false, Some(AGENT_SCREEN_TAIL_ROWS * 2))
            .await
        else {
            return evidence;
        };
        match classify_agent_screen(&tail.text) {
            Some((agent, AgentScreenState::Ready)) => LivePromptEvidence {
                shell_state: LiveShellState::AgentReady,
                last_exit_code: evidence.last_exit_code,
                detail: Some(format!("agent_ready:{agent}")),
            },
            Some((agent, state)) => LivePromptEvidence {
                shell_state: LiveShellState::CommandRunning,
                last_exit_code: evidence.last_exit_code,
                detail: Some(if state == AgentScreenState::Busy {
                    format!("agent_busy:{agent}")
                } else {
                    format!("agent_awaiting_decision:{agent}")
                }),
            },
            None => evidence,
        }
    };
    match crate::runtime_async::timeout_with_cx(cx, budget, fetch).await {
        Ok(evidence) => evidence,
        Err(error) => LivePromptEvidence::unavailable(&format_args!("semantic query: {error}")),
    }
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
    /// The live registry plus the mux it observes, for live prompt evidence
    /// (vendored capture stores rendered rows, so storage has no OSC 133).
    RegistryWithMux(
        std::sync::Arc<crate::runtime_async::RwLock<crate::ingest::PaneRegistry>>,
        crate::wezterm::WeztermHandle,
    ),
}

impl WatcherCapabilitySource {
    async fn pane_state(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
    ) -> Result<Option<IpcPaneState>, String> {
        match self {
            Self::Ipc(path) => fetch_pane_state_from_ipc(cx, path, pane_id).await,
            Self::RegistryWithMux(registry, mux) => {
                let Some(mut state) = Self::Registry(std::sync::Arc::clone(registry))
                    .registry_pane_state(cx, pane_id)
                    .await?
                else {
                    return Ok(None);
                };
                state.live_prompt = Some(
                    fetch_live_prompt_evidence(
                        cx,
                        mux,
                        pane_id,
                        std::time::Duration::from_millis(750),
                    )
                    .await,
                );
                Ok(Some(state))
            }
            Self::Registry(_) => self.registry_pane_state(cx, pane_id).await,
        }
    }

    async fn registry_pane_state(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
    ) -> Result<Option<IpcPaneState>, String> {
        match self {
            Self::Ipc(_) | Self::RegistryWithMux(..) => Ok(None),
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
                    live_prompt: None,
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
    let mut agent_composer_ready = false;

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
                    // Live semantic zones describe the pane now; stored
                    // segments only carry markers on raw-byte capture paths.
                    if let Some(live) = &state.live_prompt {
                        if let Some(live_state) = live.osc_state() {
                            osc_state = Some(live_state);
                            agent_composer_ready =
                                live.shell_state == LiveShellState::AgentReady;
                            // Name agent-screen evidence so a denial explains itself.
                            if let Some(detail) = &live.detail {
                                warnings.push(bounded_detail("Live prompt evidence: ", detail));
                            }
                        } else if let Some(detail) = &live.detail {
                            warnings.push(bounded_detail("Live prompt state unavailable: ", detail));
                        }
                    }
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
    // Full-screen agent TUIs (codex) keep their composer on the alternate
    // screen. The alt-screen gate keeps text out of vim and pagers; a screen
    // positively recognized as an agent composer at rest is expecting text.
    if agent_composer_ready && capabilities.alt_screen == Some(true) {
        capabilities.alt_screen = Some(false);
        warnings.push(
            "Alternate screen holds an idle agent composer; alt-screen gate not applied.".to_string(),
        );
    }

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
                live_prompt: None,
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

    fn zone(
        kind: crate::wezterm::MuxSemanticZoneKind,
        y: isize,
        text: &str,
    ) -> crate::wezterm::MuxSemanticZone {
        crate::wezterm::MuxSemanticZone {
            start_y: y,
            start_x: 0,
            end_y: y,
            end_x: text.len().saturating_sub(1),
            semantic_type: kind,
            text: text.to_string(),
        }
    }

    #[test]
    fn live_semantic_zones_classify_the_shell_at_the_cursor() {
        use crate::wezterm::{MuxSemanticSnapshot, MuxSemanticZoneKind as Kind};
        let classify = |zones: Vec<crate::wezterm::MuxSemanticZone>, cursor: Option<i64>| {
            LivePromptEvidence::from_semantic_zones(
                &MuxSemanticSnapshot {
                    zones,
                    last_exit_code: Some(0),
                },
                cursor,
            )
            .shell_state
        };
        // Bare shell: every cell defaults to Output.
        assert_eq!(
            classify(vec![zone(Kind::Output, 0, "% ls")], Some(0)),
            LiveShellState::NoIntegration
        );
        assert_eq!(classify(Vec::new(), None), LiveShellState::NoIntegration);
        // Fresh prompt after a finished command, cursor on the prompt line.
        let finished = vec![
            zone(Kind::Prompt, 0, "$ "),
            zone(Kind::Output, 1, "ok"),
            zone(Kind::Prompt, 2, "$ "),
        ];
        assert_eq!(classify(finished.clone(), Some(2)), LiveShellState::Prompt);
        assert_eq!(classify(finished, None), LiveShellState::Prompt);
        // Blank Output padding below the prompt is not command output.
        assert_eq!(
            classify(
                vec![zone(Kind::Prompt, 0, "$ "), zone(Kind::Output, 1, "   ")],
                Some(0)
            ),
            LiveShellState::Prompt
        );
        // Typing at the prompt.
        let typing = vec![zone(Kind::Prompt, 0, "$ "), zone(Kind::Input, 0, "ls")];
        assert_eq!(classify(typing.clone(), Some(0)), LiveShellState::Input);
        // Submitted and silent (`sleep 100`): the cursor left the input line.
        assert_eq!(classify(typing, Some(1)), LiveShellState::CommandRunning);
        // Output after the last prompt.
        assert_eq!(
            classify(
                vec![zone(Kind::Prompt, 0, "$ "), zone(Kind::Output, 1, "building")],
                Some(2)
            ),
            LiveShellState::CommandRunning
        );
    }

    fn agent_screen(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agent_screens")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    #[test]
    fn real_agent_idle_composers_read_ready() {
        // Captured from codex-cli 0.157.0 and Claude Code 2.1.282 idle in a
        // trusted folder (ft-t4sma); no input was sent.
        assert_eq!(
            classify_agent_screen(&agent_screen("codex_0157_idle.txt")),
            Some(("codex", AgentScreenState::Ready))
        );
        assert_eq!(
            classify_agent_screen(&agent_screen("claude_code_2_1_idle.txt")),
            Some(("claude_code", AgentScreenState::Ready))
        );
    }

    #[test]
    fn real_agent_trust_menus_await_a_decision_and_are_never_ready() {
        // Typing text plus Enter into these menus would pick an option.
        for (fixture, agent) in [
            ("codex_0157_trust_menu.txt", "codex"),
            ("claude_code_2_1_trust_menu.txt", "claude_code"),
            ("gemini_trust_menu.txt", "gemini"),
        ] {
            assert_eq!(
                classify_agent_screen(&agent_screen(fixture)),
                Some((agent, AgentScreenState::AwaitingDecision)),
                "{fixture}"
            );
        }
    }

    #[test]
    fn busy_and_permission_screens_are_not_ready() {
        // Synthetic, in the agents' documented layouts: working status lines
        // sit above a still-visible composer.
        let codex_busy = "• Working (12s • esc to interrupt)\n\n› Ask Codex to do anything\n  ? for shortcuts\n";
        assert_eq!(
            classify_agent_screen(codex_busy),
            Some(("codex", AgentScreenState::Busy))
        );
        let claude_busy = "✻ Pondering… (8s · esc to interrupt)\n────────\n❯ \n────────\n  ? for shortcuts\n";
        assert_eq!(
            classify_agent_screen(claude_busy),
            Some(("claude_code", AgentScreenState::Busy))
        );
        let claude_permission = "Claude Code\n Do you want to make this edit to main.rs?\n ❯ 1. Yes\n   2. Yes, allow all edits\n   3. No\n";
        assert_eq!(
            classify_agent_screen(claude_permission),
            Some(("claude_code", AgentScreenState::AwaitingDecision))
        );
        // Plain shells and unknown programs are not agent screens.
        assert_eq!(classify_agent_screen("$ ls\nfoo bar\n$ "), None);
        assert_eq!(classify_agent_screen(""), None);
    }

    #[test]
    fn only_an_idle_agent_composer_lifts_the_alt_screen_gate() {
        let state = |pane_id, shell_state, detail: &str| IpcPaneState {
            pane_id,
            known: true,
            observed: Some(true),
            alt_screen: None,
            last_status_at: None,
            in_gap: Some(false),
            cursor_alt_screen: Some(true),
            reason: None,
            live_prompt: Some(LivePromptEvidence {
                shell_state,
                last_exit_code: None,
                detail: Some(detail.to_string()),
            }),
        };
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let resolve = |pane_id| {
            runtime.block_on(resolve_pane_capabilities(
                &crate::cx::for_testing(),
                pane_id,
                None,
                Some(Path::new("/nonexistent/ft-capability-test.sock")),
            ))
        };

        let _ready = set_test_pane_state_override(state(
            4_501,
            LiveShellState::AgentReady,
            "agent_ready:codex",
        ));
        let ready = resolve(4_501);
        assert!(ready.capabilities.prompt_active);
        assert_eq!(ready.capabilities.alt_screen, Some(false));

        // A busy agent (or vim, or a pager) keeps the alt-screen gate.
        let _busy = set_test_pane_state_override(state(
            4_502,
            LiveShellState::CommandRunning,
            "agent_busy:codex",
        ));
        let busy = resolve(4_502);
        assert_eq!(busy.capabilities.alt_screen, Some(true));
        assert!(busy.capabilities.command_running);
    }

    #[test]
    fn agent_ready_is_prompt_evidence_and_agent_busy_is_a_running_command() {
        let ready = LivePromptEvidence {
            shell_state: LiveShellState::AgentReady,
            last_exit_code: None,
            detail: Some("agent_ready:codex".to_string()),
        };
        let caps = PaneCapabilities::from_ingest_state(ready.osc_state().as_ref(), Some(false), false);
        assert!(caps.prompt_active && !caps.command_running);
        let json = serde_json::to_value(&ready).unwrap();
        assert_eq!(json["shell_state"], "agent_ready");
    }

    #[test]
    fn live_prompt_evidence_maps_onto_policy_capabilities() {
        let caps = |shell_state| {
            let evidence = LivePromptEvidence {
                shell_state,
                last_exit_code: None,
                detail: None,
            };
            PaneCapabilities::from_ingest_state(evidence.osc_state().as_ref(), Some(false), false)
        };
        assert!(caps(LiveShellState::Prompt).prompt_active);
        assert!(caps(LiveShellState::Input).prompt_active);
        let running = caps(LiveShellState::CommandRunning);
        assert!(!running.prompt_active && running.command_running);
        for unknown in [LiveShellState::NoIntegration, LiveShellState::Unavailable] {
            let caps = caps(unknown);
            assert!(!caps.prompt_active && !caps.command_running);
        }
    }

    #[test]
    fn watcher_live_prompt_is_prompt_evidence_without_stored_markers() {
        let state = |pane_id, shell_state| IpcPaneState {
            pane_id,
            known: true,
            observed: Some(true),
            alt_screen: None,
            last_status_at: None,
            in_gap: Some(false),
            cursor_alt_screen: Some(false),
            reason: None,
            live_prompt: Some(LivePromptEvidence {
                shell_state,
                last_exit_code: None,
                detail: (shell_state == LiveShellState::Unavailable)
                    .then(|| "mux gone".to_string()),
            }),
        };
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let resolve = |pane_id| {
            runtime.block_on(resolve_pane_capabilities(
                &crate::cx::for_testing(),
                pane_id,
                None,
                Some(Path::new("/nonexistent/ft-capability-test.sock")),
            ))
        };

        let _prompt = set_test_pane_state_override(state(4_401, LiveShellState::Prompt));
        let prompt = resolve(4_401);
        assert!(prompt.capabilities.prompt_active);
        assert_eq!(prompt.capabilities.alt_screen, Some(false));

        let _running = set_test_pane_state_override(state(4_402, LiveShellState::CommandRunning));
        assert!(resolve(4_402).capabilities.command_running);

        let _down = set_test_pane_state_override(state(4_403, LiveShellState::Unavailable));
        let down = resolve(4_403);
        assert!(!down.capabilities.prompt_active);
        assert!(
            down.warnings
                .iter()
                .any(|warning| warning == "Live prompt state unavailable: mux gone"),
            "{:?}",
            down.warnings
        );
    }

    #[test]
    fn pane_state_json_without_live_prompt_still_parses() {
        let state: IpcPaneState = serde_json::from_value(serde_json::json!({
            "pane_id": 1, "known": true, "live_prompt": null
        }))
        .unwrap();
        assert_eq!(state.live_prompt, None);
        let state: IpcPaneState = serde_json::from_value(serde_json::json!({
            "pane_id": 1, "known": true,
            "live_prompt": {"shell_state": "prompt", "last_exit_code": 0}
        }))
        .unwrap();
        assert_eq!(
            state.live_prompt.map(|live| live.shell_state),
            Some(LiveShellState::Prompt)
        );
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
            live_prompt: None,
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
            live_prompt: None,
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
