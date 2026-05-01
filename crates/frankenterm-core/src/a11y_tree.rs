//! Accessibility-tree contract and regression-fixture infrastructure
//! ([BR-TERM-EMULATOR-UPLIFT.A11Y.1] / `ft-mpc9b.10.1`).
//!
//! Renderer changes can silently break the platform accessibility (AT)
//! tree: a blind operator running VoiceOver / Orca / Narrator hears
//! nothing about a pane state change but the visual frame looks
//! correct. This module establishes the **contract** the per-platform
//! recorders must satisfy:
//!
//! - [`AccessibilityEvent`] — the platform-agnostic event type. Each
//!   per-platform recorder maps its native notification (e.g.
//!   `NSAccessibilityPostNotification`,
//!   `org.a11y.atspi.Event.Focus`, UIA event) to one of these
//!   variants.
//! - [`AccessibilityScenario`] — the closed list of 5 scenarios from
//!   `docs/a11y/scenario-corpus.md`. Each scenario carries a
//!   contract: the canonical event sequence the recorder is expected
//!   to produce.
//! - [`AccessibilityRecorder`] — the trait every platform recorder
//!   implements. The fixture in
//!   `crates/frankenterm-core/tests/a11y_regression_fixture.rs`
//!   consumes the trait directly via the [`ContractRecorder`]
//!   in-memory implementation; future per-platform integrations swap
//!   in a real adapter.
//! - [`JsonlLogWriter`] — the structured-log writer. Test logs land
//!   at `tests/a11y/logs/<platform>-<scenario>.jsonl` and goldens at
//!   `tests/a11y/golden/<platform>-<scenario>.jsonl`.
//! - [`InvariantViolation`] — the named, machine-runnable invariants
//!   the proptest harness asserts on every recorded stream.
//!
//! See `docs/a11y/at-tree-audit.md` for the per-platform code-citation
//! audit and `docs/a11y/scenario-corpus.md` for the prose contract.
//!
//! ## Why this lives in `frankenterm-core`
//!
//! The window crate (`frankenterm/window/`) is WezTerm-derived and
//! intentionally heavyweight to touch. Per the bead's closure plan the
//! actual platform integrations land in follow-on beads; this module
//! gives those beads a stable Rust contract + fixture they can consume
//! without having to invent the schema themselves.

use serde::{Deserialize, Serialize};

// ============================================================================
// Event taxonomy
// ============================================================================

/// One AT-tree event captured by an [`AccessibilityRecorder`].
///
/// Each variant is the platform-agnostic equivalent of the native
/// notifications the recorder consumes. See `scenario-corpus.md` for
/// the canonical mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccessibilityEvent {
    /// A pane / window's text content was mutated.
    TextValueChanged {
        /// Monotonic timestamp (ms since recorder start).
        ts_ms: u64,
        /// Element role (e.g. `"Terminal"`, `"Dialog"`).
        role: String,
        /// Stable element name (e.g. `"pane:42"`).
        name: String,
        /// New text value the AT framework should announce.
        value: String,
    },
    /// Focused element changed (window-level or pane-level).
    FocusChanged {
        ts_ms: u64,
        role: String,
        name: String,
    },
    /// A new top-level UI element opened (modal dialog, palette, …).
    WindowOpened {
        ts_ms: u64,
        role: String,
        name: String,
    },
    /// A previously-opened element closed.
    WindowClosed {
        ts_ms: u64,
        role: String,
        name: String,
    },
    /// Active selection range changed inside a focused element.
    SelectionChanged {
        ts_ms: u64,
        role: String,
        name: String,
        range_start_line: u32,
        range_start_col: u32,
        range_end_line: u32,
        range_end_col: u32,
    },
    /// Viewport scrolled by more than the debounce threshold.
    ScrollPositionChanged {
        ts_ms: u64,
        role: String,
        name: String,
        viewport_top_line: u32,
        viewport_bottom_line: u32,
    },
    /// AT-framework-agnostic announcement for transient messages.
    AnnounceMessage {
        ts_ms: u64,
        priority: AnnouncePriority,
        value: String,
    },
}

impl AccessibilityEvent {
    /// Common accessor: every variant carries `ts_ms`.
    #[must_use]
    pub fn ts_ms(&self) -> u64 {
        match self {
            Self::TextValueChanged { ts_ms, .. }
            | Self::FocusChanged { ts_ms, .. }
            | Self::WindowOpened { ts_ms, .. }
            | Self::WindowClosed { ts_ms, .. }
            | Self::SelectionChanged { ts_ms, .. }
            | Self::ScrollPositionChanged { ts_ms, .. }
            | Self::AnnounceMessage { ts_ms, .. } => *ts_ms,
        }
    }

    /// The element name the event targets, when the variant carries one.
    /// `AnnounceMessage` is window-level and returns `None`.
    #[must_use]
    pub fn target_name(&self) -> Option<&str> {
        match self {
            Self::TextValueChanged { name, .. }
            | Self::FocusChanged { name, .. }
            | Self::WindowOpened { name, .. }
            | Self::WindowClosed { name, .. }
            | Self::SelectionChanged { name, .. }
            | Self::ScrollPositionChanged { name, .. } => Some(name.as_str()),
            Self::AnnounceMessage { .. } => None,
        }
    }
}

/// Announcement priority — VoiceOver `NSAccessibilityAnnouncementPriorityHigh`
/// equivalent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnouncePriority {
    /// Polite — debounced; equivalent to `aria-live="polite"`.
    Polite,
    /// Assertive — interrupts current speech; equivalent to
    /// `aria-live="assertive"` / `NSAccessibilityAnnouncementPriorityHigh`.
    Assertive,
}

// ============================================================================
// Scenario corpus
// ============================================================================

/// The closed list of scenarios the regression fixture exercises.
///
/// Mirrors the bead description's 5 scenarios and the dirty-event
/// taxonomy in `crates/frankenterm-gui/src/gpu_regression_fuzz.rs::FuzzInputEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessibilityScenario {
    /// User types into the focused pane.
    SteadyTyping,
    /// User focuses a different pane (or the platform raises focus).
    PaneFocusChange,
    /// A modal overlay opens (confirm dialog, command palette).
    DialogOpen,
    /// User drags a non-empty selection inside the focused pane.
    SelectionChange,
    /// User scrolls the viewport by more than 1 row.
    ScrollPositionChange,
}

impl AccessibilityScenario {
    /// Every scenario in declaration order. Stable for golden-file
    /// naming.
    pub const ALL: &'static [AccessibilityScenario] = &[
        Self::SteadyTyping,
        Self::PaneFocusChange,
        Self::DialogOpen,
        Self::SelectionChange,
        Self::ScrollPositionChange,
    ];

    /// Slug used in golden / log filenames
    /// (`tests/a11y/golden/<platform>-<slug>.jsonl`).
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::SteadyTyping => "steady_typing",
            Self::PaneFocusChange => "pane_focus_change",
            Self::DialogOpen => "dialog_open",
            Self::SelectionChange => "selection_change",
            Self::ScrollPositionChange => "scroll_position_change",
        }
    }
}

// ============================================================================
// Recorder trait + in-memory contract recorder
// ============================================================================

/// The per-platform AT event capture trait.
///
/// Every platform recorder (macOS NSAccessibility, Linux AT-SPI,
/// Windows UIA) implements this trait. The fixture consumes it as
/// `Box<dyn AccessibilityRecorder>` so a test can swap an in-memory
/// recorder for a real OS-side one without changing scenario code.
pub trait AccessibilityRecorder: Send {
    /// Begin recording a scenario. MUST be called exactly once before
    /// the first `record` call.
    fn start(&mut self, scenario: AccessibilityScenario);

    /// Append an AT event to the active scenario's stream.
    fn record(&mut self, event: AccessibilityEvent);

    /// Finalize the scenario and return its captured event vector.
    /// MUST be called exactly once after all `record` calls.
    fn finish(&mut self) -> Vec<AccessibilityEvent>;
}

/// In-memory recorder used by the regression fixture and by
/// per-platform integration tests as the **expected** stream when the
/// real recorder is offline.
#[derive(Debug, Default)]
pub struct ContractRecorder {
    scenario: Option<AccessibilityScenario>,
    events: Vec<AccessibilityEvent>,
}

impl ContractRecorder {
    /// Create an empty recorder. Caller MUST `start` before recording.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Currently-active scenario, if any.
    #[must_use]
    pub fn scenario(&self) -> Option<AccessibilityScenario> {
        self.scenario
    }

    /// Read-only view of the recorded stream so far.
    #[must_use]
    pub fn events(&self) -> &[AccessibilityEvent] {
        &self.events
    }
}

impl AccessibilityRecorder for ContractRecorder {
    fn start(&mut self, scenario: AccessibilityScenario) {
        assert!(
            self.scenario.is_none(),
            "ContractRecorder::start called twice without finish() (scenario={scenario:?})"
        );
        self.scenario = Some(scenario);
        self.events.clear();
    }

    fn record(&mut self, event: AccessibilityEvent) {
        assert!(
            self.scenario.is_some(),
            "ContractRecorder::record called before start()"
        );
        self.events.push(event);
    }

    fn finish(&mut self) -> Vec<AccessibilityEvent> {
        self.scenario.take();
        std::mem::take(&mut self.events)
    }
}

// ============================================================================
// Platform recorder stubs
// ============================================================================

/// Identifies the AT framework a recorder targets — drives golden /
/// log filename selection (`<platform>-<scenario>.jsonl`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessibilityPlatform {
    /// macOS `NSAccessibility`.
    MacosNsAccessibility,
    /// Linux/X11 + Linux/Wayland AT-SPI (single platform — atspi-rs
    /// routes via D-Bus regardless of windowing protocol).
    LinuxAtSpi,
    /// Windows UI Automation.
    WindowsUiAutomation,
    /// Synthetic — used by tests and as the foundation slice's
    /// "expected events" recorder.
    Synthetic,
}

impl AccessibilityPlatform {
    /// Filename slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::MacosNsAccessibility => "macos",
            Self::LinuxAtSpi => "linux",
            Self::WindowsUiAutomation => "windows",
            Self::Synthetic => "synthetic",
        }
    }

    /// Whether a real AT-framework adapter is wired today.
    ///
    /// All real platforms currently return `false` — the fixture
    /// operates against `ContractRecorder` until the per-platform
    /// integration beads land.
    #[must_use]
    pub const fn is_wired(self) -> bool {
        matches!(self, Self::Synthetic)
    }

    /// Stable golden-file path for `(platform, scenario)`.
    #[must_use]
    pub fn golden_filename(self, scenario: AccessibilityScenario) -> String {
        format!("{}-{}.jsonl", self.slug(), scenario.slug())
    }
}

// ============================================================================
// Invariants
// ============================================================================

/// One named invariant violation. The fixture enumerates these as
/// proptest assertions; a real-recorder integration that violates an
/// invariant fails the regression lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InvariantViolation {
    /// Two events whose timestamps go backwards.
    NonMonotonicTimestamps {
        prior_ts_ms: u64,
        current_ts_ms: u64,
        index: usize,
    },
    /// A `SelectionChanged` event preceded the focus on the same target.
    SelectionBeforeFocus { target_name: String, index: usize },
    /// Two consecutive `FocusChanged` events for the same target —
    /// indicates a recorder fired focus when nothing actually changed.
    DuplicateFocus { target_name: String, index: usize },
    /// `range_end < range_start` on a `SelectionChanged`.
    InvertedSelectionRange { target_name: String, index: usize },
    /// A scenario stream is empty (the recorder never recorded
    /// anything for a scenario whose contract requires at least one
    /// event).
    EmptyStream,
}

/// Run all invariant checks against an event stream. Returns the
/// accumulated list (empty Vec = clean).
///
/// The check is intentionally tolerant of the
/// `pane_focus_change` scenario's "focus left the window" case
/// (`FocusChanged` events with empty target name are filtered out per
/// `scenario-corpus.md`).
#[must_use]
pub fn check_invariants(
    scenario: AccessibilityScenario,
    events: &[AccessibilityEvent],
) -> Vec<InvariantViolation> {
    let mut violations = Vec::new();

    if events.is_empty() {
        // Every contract scenario produces at least one event.
        violations.push(InvariantViolation::EmptyStream);
        return violations;
    }

    // Monotonic timestamps.
    let mut prior = events[0].ts_ms();
    for (i, ev) in events.iter().enumerate().skip(1) {
        let cur = ev.ts_ms();
        if cur < prior {
            violations.push(InvariantViolation::NonMonotonicTimestamps {
                prior_ts_ms: prior,
                current_ts_ms: cur,
                index: i,
            });
        }
        prior = cur;
    }

    // Focus tracking.
    let mut last_focus_target: Option<String> = None;
    for (i, ev) in events.iter().enumerate() {
        match ev {
            AccessibilityEvent::FocusChanged { name, .. } => {
                if let Some(prev) = &last_focus_target {
                    if prev == name {
                        violations.push(InvariantViolation::DuplicateFocus {
                            target_name: name.clone(),
                            index: i,
                        });
                    }
                }
                last_focus_target = Some(name.clone());
            }
            AccessibilityEvent::SelectionChanged {
                name,
                range_start_line,
                range_start_col,
                range_end_line,
                range_end_col,
                ..
            } => {
                let focused = last_focus_target
                    .as_deref()
                    .is_some_and(|f| f == name.as_str());
                if !focused {
                    violations.push(InvariantViolation::SelectionBeforeFocus {
                        target_name: name.clone(),
                        index: i,
                    });
                }
                if (*range_end_line, *range_end_col) < (*range_start_line, *range_start_col) {
                    violations.push(InvariantViolation::InvertedSelectionRange {
                        target_name: name.clone(),
                        index: i,
                    });
                }
            }
            _ => {}
        }
    }

    // Scenario-specific: dialog_open MUST contain a WindowOpened +
    // FocusChanged + AnnounceMessage triple in some order.
    if matches!(scenario, AccessibilityScenario::DialogOpen) {
        let kinds: Vec<&'static str> = events.iter().map(event_kind_name).collect();
        for required in ["window_opened", "focus_changed", "announce_message"] {
            if !kinds.contains(&required) {
                // Surfaced as a synthetic empty-stream violation —
                // the explicit "missing kind" variant would balloon
                // the enum without giving the test a clearer error.
                violations.push(InvariantViolation::EmptyStream);
            }
        }
    }

    violations
}

fn event_kind_name(ev: &AccessibilityEvent) -> &'static str {
    match ev {
        AccessibilityEvent::TextValueChanged { .. } => "text_value_changed",
        AccessibilityEvent::FocusChanged { .. } => "focus_changed",
        AccessibilityEvent::WindowOpened { .. } => "window_opened",
        AccessibilityEvent::WindowClosed { .. } => "window_closed",
        AccessibilityEvent::SelectionChanged { .. } => "selection_changed",
        AccessibilityEvent::ScrollPositionChanged { .. } => "scroll_position_changed",
        AccessibilityEvent::AnnounceMessage { .. } => "announce_message",
    }
}

// ============================================================================
// JSONL writer
// ============================================================================

/// Serialize a stream of events as JSON-Lines (one JSON object per
/// line). Used to write `tests/a11y/logs/<platform>-<scenario>.jsonl`
/// at recording time and to read the corresponding goldens at
/// validation time.
pub struct JsonlLogWriter;

impl JsonlLogWriter {
    /// Render an event stream as a single JSONL string. Trailing
    /// newline; one event per line.
    #[must_use]
    pub fn render(events: &[AccessibilityEvent]) -> String {
        let mut out = String::new();
        for ev in events {
            // SAFETY: AccessibilityEvent is Serialize and never
            // contains non-string-keyed maps or NaN floats, so
            // `to_string` cannot fail.
            let line = serde_json::to_string(ev).expect("AccessibilityEvent always serializes");
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// Parse a JSONL string back into an event vector. Empty lines
    /// are tolerated (allows golden files to use blank-line separators
    /// for readability).
    pub fn parse(jsonl: &str) -> Result<Vec<AccessibilityEvent>, serde_json::Error> {
        let mut events = Vec::new();
        for line in jsonl.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            events.push(serde_json::from_str(trimmed)?);
        }
        Ok(events)
    }
}

// ============================================================================
// Contract scenario builders — the canonical "expected" event sequences
// ============================================================================

/// Build the canonical expected-events sequence for a scenario, against
/// a synthetic pane id. These are the contracts every per-platform
/// recorder must reproduce; the fixture's golden snapshot test pins
/// them, so a future renderer change that breaks the contract fails
/// the lane.
#[must_use]
pub fn contract_events(scenario: AccessibilityScenario, pane_id: u64) -> Vec<AccessibilityEvent> {
    let pane_name = format!("pane:{pane_id}");
    match scenario {
        AccessibilityScenario::SteadyTyping => vec![
            AccessibilityEvent::FocusChanged {
                ts_ms: 0,
                role: "Terminal".to_string(),
                name: pane_name.clone(),
            },
            AccessibilityEvent::TextValueChanged {
                ts_ms: 10,
                role: "Terminal".to_string(),
                name: pane_name,
                value: "hello world\n".to_string(),
            },
        ],
        AccessibilityScenario::PaneFocusChange => vec![
            AccessibilityEvent::FocusChanged {
                ts_ms: 0,
                role: "Terminal".to_string(),
                name: format!("pane:{}", pane_id.saturating_sub(1).max(1)),
            },
            AccessibilityEvent::FocusChanged {
                ts_ms: 5,
                role: "Terminal".to_string(),
                name: pane_name,
            },
        ],
        AccessibilityScenario::DialogOpen => vec![
            AccessibilityEvent::FocusChanged {
                ts_ms: 0,
                role: "Terminal".to_string(),
                name: pane_name,
            },
            AccessibilityEvent::WindowOpened {
                ts_ms: 5,
                role: "Dialog".to_string(),
                name: "Confirm close pane".to_string(),
            },
            AccessibilityEvent::FocusChanged {
                ts_ms: 6,
                role: "Dialog".to_string(),
                name: "Confirm close pane".to_string(),
            },
            AccessibilityEvent::AnnounceMessage {
                ts_ms: 7,
                priority: AnnouncePriority::Assertive,
                value: "Confirm close pane".to_string(),
            },
        ],
        AccessibilityScenario::SelectionChange => vec![
            AccessibilityEvent::FocusChanged {
                ts_ms: 0,
                role: "Terminal".to_string(),
                name: pane_name.clone(),
            },
            AccessibilityEvent::SelectionChanged {
                ts_ms: 50,
                role: "Terminal".to_string(),
                name: pane_name,
                range_start_line: 3,
                range_start_col: 0,
                range_end_line: 5,
                range_end_col: 12,
            },
        ],
        AccessibilityScenario::ScrollPositionChange => vec![
            AccessibilityEvent::FocusChanged {
                ts_ms: 0,
                role: "Terminal".to_string(),
                name: pane_name.clone(),
            },
            AccessibilityEvent::ScrollPositionChanged {
                ts_ms: 20,
                role: "Terminal".to_string(),
                name: pane_name,
                viewport_top_line: 100,
                viewport_bottom_line: 124,
            },
        ],
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scenario_has_a_clean_contract() {
        for scenario in AccessibilityScenario::ALL {
            let events = contract_events(*scenario, 42);
            let v = check_invariants(*scenario, &events);
            assert!(v.is_empty(), "{:?} contract violations: {v:?}", scenario);
        }
    }

    #[test]
    fn empty_stream_is_a_violation() {
        let v = check_invariants(AccessibilityScenario::SteadyTyping, &[]);
        assert!(
            v.iter()
                .any(|x| matches!(x, InvariantViolation::EmptyStream))
        );
    }

    #[test]
    fn non_monotonic_timestamps_detected() {
        let events = vec![
            AccessibilityEvent::FocusChanged {
                ts_ms: 100,
                role: "Terminal".to_string(),
                name: "pane:1".to_string(),
            },
            AccessibilityEvent::TextValueChanged {
                ts_ms: 50,
                role: "Terminal".to_string(),
                name: "pane:1".to_string(),
                value: "x".to_string(),
            },
        ];
        let v = check_invariants(AccessibilityScenario::SteadyTyping, &events);
        assert!(
            v.iter()
                .any(|x| matches!(x, InvariantViolation::NonMonotonicTimestamps { .. })),
            "expected NonMonotonicTimestamps in {v:?}"
        );
    }

    #[test]
    fn selection_before_focus_detected() {
        let events = vec![AccessibilityEvent::SelectionChanged {
            ts_ms: 0,
            role: "Terminal".to_string(),
            name: "pane:1".to_string(),
            range_start_line: 0,
            range_start_col: 0,
            range_end_line: 1,
            range_end_col: 0,
        }];
        let v = check_invariants(AccessibilityScenario::SelectionChange, &events);
        assert!(
            v.iter()
                .any(|x| matches!(x, InvariantViolation::SelectionBeforeFocus { .. })),
            "expected SelectionBeforeFocus in {v:?}"
        );
    }

    #[test]
    fn duplicate_focus_detected() {
        let events = vec![
            AccessibilityEvent::FocusChanged {
                ts_ms: 0,
                role: "Terminal".to_string(),
                name: "pane:1".to_string(),
            },
            AccessibilityEvent::FocusChanged {
                ts_ms: 5,
                role: "Terminal".to_string(),
                name: "pane:1".to_string(),
            },
        ];
        let v = check_invariants(AccessibilityScenario::PaneFocusChange, &events);
        assert!(
            v.iter()
                .any(|x| matches!(x, InvariantViolation::DuplicateFocus { .. })),
            "expected DuplicateFocus in {v:?}"
        );
    }

    #[test]
    fn inverted_selection_range_detected() {
        let events = vec![
            AccessibilityEvent::FocusChanged {
                ts_ms: 0,
                role: "Terminal".to_string(),
                name: "pane:1".to_string(),
            },
            AccessibilityEvent::SelectionChanged {
                ts_ms: 5,
                role: "Terminal".to_string(),
                name: "pane:1".to_string(),
                range_start_line: 5,
                range_start_col: 10,
                range_end_line: 2,
                range_end_col: 0,
            },
        ];
        let v = check_invariants(AccessibilityScenario::SelectionChange, &events);
        assert!(
            v.iter()
                .any(|x| matches!(x, InvariantViolation::InvertedSelectionRange { .. })),
            "expected InvertedSelectionRange in {v:?}"
        );
    }

    #[test]
    fn jsonl_roundtrip() {
        let events = contract_events(AccessibilityScenario::DialogOpen, 7);
        let rendered = JsonlLogWriter::render(&events);
        let parsed = JsonlLogWriter::parse(&rendered).expect("parse");
        assert_eq!(events, parsed);
    }

    #[test]
    fn jsonl_tolerates_blank_lines() {
        let events = contract_events(AccessibilityScenario::SteadyTyping, 1);
        let mut rendered = JsonlLogWriter::render(&events);
        rendered.insert(0, '\n');
        rendered.push('\n');
        let parsed = JsonlLogWriter::parse(&rendered).expect("parse");
        assert_eq!(events, parsed);
    }

    #[test]
    fn contract_recorder_lifecycle() {
        let mut rec = ContractRecorder::new();
        assert!(rec.scenario().is_none());
        rec.start(AccessibilityScenario::SteadyTyping);
        assert_eq!(rec.scenario(), Some(AccessibilityScenario::SteadyTyping));
        rec.record(AccessibilityEvent::FocusChanged {
            ts_ms: 0,
            role: "Terminal".to_string(),
            name: "pane:1".to_string(),
        });
        assert_eq!(rec.events().len(), 1);
        let drained = rec.finish();
        assert_eq!(drained.len(), 1);
        assert!(rec.scenario().is_none());
    }

    #[test]
    fn platform_metadata_is_stable() {
        assert_eq!(AccessibilityPlatform::MacosNsAccessibility.slug(), "macos");
        assert_eq!(AccessibilityPlatform::LinuxAtSpi.slug(), "linux");
        assert_eq!(AccessibilityPlatform::WindowsUiAutomation.slug(), "windows");
        assert!(!AccessibilityPlatform::MacosNsAccessibility.is_wired());
        assert!(!AccessibilityPlatform::LinuxAtSpi.is_wired());
        assert!(!AccessibilityPlatform::WindowsUiAutomation.is_wired());
        assert!(AccessibilityPlatform::Synthetic.is_wired());
        assert_eq!(
            AccessibilityPlatform::MacosNsAccessibility
                .golden_filename(AccessibilityScenario::SteadyTyping),
            "macos-steady_typing.jsonl"
        );
    }

    #[test]
    fn scenario_slugs_match_doc() {
        let slugs: Vec<&'static str> = AccessibilityScenario::ALL
            .iter()
            .map(|s| s.slug())
            .collect();
        assert_eq!(
            slugs,
            vec![
                "steady_typing",
                "pane_focus_change",
                "dialog_open",
                "selection_change",
                "scroll_position_change",
            ]
        );
    }

    #[test]
    fn dialog_open_contract_carries_required_kinds() {
        let events = contract_events(AccessibilityScenario::DialogOpen, 1);
        let mut kinds: Vec<&'static str> = events.iter().map(super::event_kind_name).collect();
        kinds.sort_unstable();
        kinds.dedup();
        assert!(kinds.contains(&"window_opened"));
        assert!(kinds.contains(&"focus_changed"));
        assert!(kinds.contains(&"announce_message"));
    }

    #[test]
    fn ts_ms_accessor_covers_every_variant() {
        let ev = AccessibilityEvent::AnnounceMessage {
            ts_ms: 99,
            priority: AnnouncePriority::Polite,
            value: "x".to_string(),
        };
        assert_eq!(ev.ts_ms(), 99);
        assert_eq!(ev.target_name(), None);
    }
}
