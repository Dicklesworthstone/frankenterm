//! Simulation scenario system for testing and demos.
//!
//! Defines declarative YAML scenarios that can be applied to a
//! [`MockWezterm`](crate::wezterm::MockWezterm) for reproducible testing
//! and interactive demonstrations.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::wezterm::{MockEvent, MockPane, MockWezterm};

// ---------------------------------------------------------------------------
// Scenario types
// ---------------------------------------------------------------------------

/// A declarative test/demo scenario loaded from YAML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    /// Unique scenario name.
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Total scenario duration (e.g., "30s", "2m").
    #[serde(deserialize_with = "deserialize_duration")]
    pub duration: Duration,
    /// Pane definitions (created at scenario start).
    #[serde(default)]
    pub panes: Vec<ScenarioPane>,
    /// Timed events injected during scenario execution.
    #[serde(default)]
    pub events: Vec<ScenarioEvent>,
    /// Expected outcomes to verify after execution.
    #[serde(default)]
    pub expectations: Vec<Expectation>,
}

/// A pane to create at the start of the scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioPane {
    /// Pane ID (must be unique within the scenario).
    pub id: u64,
    /// Pane title.
    #[serde(default = "default_title")]
    pub title: String,
    /// Domain name.
    #[serde(default = "default_domain")]
    pub domain: String,
    /// Current working directory.
    #[serde(default = "default_cwd")]
    pub cwd: String,
    /// Terminal columns.
    #[serde(default = "default_cols")]
    pub cols: u32,
    /// Terminal rows.
    #[serde(default = "default_rows")]
    pub rows: u32,
    /// Initial text content.
    #[serde(default)]
    pub initial_content: String,
}

fn default_title() -> String {
    "pane".to_string()
}
fn default_domain() -> String {
    "local".to_string()
}
fn default_cwd() -> String {
    "/home/user".to_string()
}
fn default_cols() -> u32 {
    80
}
fn default_rows() -> u32 {
    24
}

/// A timed event to inject during scenario execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioEvent {
    /// When to fire this event (e.g., "2s", "1m30s").
    #[serde(deserialize_with = "deserialize_duration")]
    pub at: Duration,
    /// Target pane ID.
    pub pane: u64,
    /// Action to perform.
    pub action: EventAction,
    /// Content for append/set actions.
    #[serde(default)]
    pub content: String,
    /// Name for marker actions.
    #[serde(default)]
    pub name: String,
    /// Optional comment (ignored at runtime).
    #[serde(default)]
    pub comment: Option<String>,
}

/// The kind of action a scenario event performs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventAction {
    /// Append text to the pane's content.
    Append,
    /// Clear the pane's screen.
    Clear,
    /// Set the pane's title. Uses `content` as the new title.
    SetTitle,
    /// Resize the pane. Uses `content` as "COLSxROWS".
    Resize,
    /// Insert a named marker (for expectations).
    Marker,
}

/// An expected outcome to verify after scenario execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Expectation {
    /// Type of expectation.
    #[serde(flatten)]
    pub kind: ExpectationKind,
}

/// The specific type of expectation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectationKind {
    /// Expect a pattern detection event.
    Event {
        /// Rule ID or event type to match.
        event: String,
        /// Approximate detection time.
        #[serde(default)]
        detected_at: Option<String>,
    },
    /// Expect a workflow to be triggered.
    Workflow {
        /// Workflow name.
        workflow: String,
        /// Approximate start time.
        #[serde(default)]
        started_at: Option<String>,
    },
    /// Expect pane content to contain a string.
    Contains {
        /// Pane ID to check.
        pane: u64,
        /// Text to look for.
        text: String,
    },
}

// ---------------------------------------------------------------------------
// Scenario loading and validation
// ---------------------------------------------------------------------------

impl Scenario {
    /// Load a scenario from a YAML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::from_yaml(&content)
    }

    /// Parse a scenario from a YAML string.
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        let scenario: Scenario = serde_yaml::from_str(yaml)
            .map_err(|e| crate::Error::Runtime(format!("Failed to parse scenario YAML: {e}")))?;
        scenario.validate()?;
        Ok(scenario)
    }

    /// Validate scenario consistency.
    pub fn validate(&self) -> Result<()> {
        // Check pane IDs are unique
        let mut seen_ids = std::collections::HashSet::new();
        for pane in &self.panes {
            if !seen_ids.insert(pane.id) {
                return Err(crate::Error::Runtime(format!(
                    "Duplicate pane ID {} in scenario '{}'",
                    pane.id, self.name
                )));
            }
        }

        // Check events reference valid panes
        for event in &self.events {
            if !seen_ids.contains(&event.pane) {
                return Err(crate::Error::Runtime(format!(
                    "Event at {:?} references unknown pane {} in scenario '{}'",
                    event.at, event.pane, self.name
                )));
            }
        }

        // Check events are in chronological order
        for window in self.events.windows(2) {
            if window[1].at < window[0].at {
                return Err(crate::Error::Runtime(format!(
                    "Events out of order: {:?} before {:?} in scenario '{}'",
                    window[0].at, window[1].at, self.name
                )));
            }
        }

        Ok(())
    }

    /// Apply scenario panes and initial content to a MockWezterm.
    pub async fn setup(&self, mock: &MockWezterm) -> Result<()> {
        for pane_def in &self.panes {
            let pane = MockPane {
                pane_id: pane_def.id,
                window_id: 0,
                tab_id: 0,
                title: pane_def.title.clone(),
                domain: pane_def.domain.clone(),
                cwd: pane_def.cwd.clone(),
                is_active: pane_def.id == 0,
                is_zoomed: false,
                cols: pane_def.cols,
                rows: pane_def.rows,
                content: pane_def.initial_content.clone(),
            };
            mock.add_pane(pane).await;
        }
        Ok(())
    }

    /// Convert a scenario event to a MockEvent for injection.
    pub fn to_mock_event(event: &ScenarioEvent) -> Result<MockEvent> {
        match event.action {
            EventAction::Append => Ok(MockEvent::AppendOutput(event.content.clone())),
            EventAction::Clear => Ok(MockEvent::ClearScreen),
            EventAction::SetTitle => Ok(MockEvent::SetTitle(event.content.clone())),
            EventAction::Resize => {
                let parts: Vec<&str> = event.content.split('x').collect();
                if parts.len() != 2 {
                    return Err(crate::Error::Runtime(format!(
                        "Resize content must be 'COLSxROWS', got '{}'",
                        event.content
                    )));
                }
                let cols: u32 = parts[0].trim().parse().map_err(|_| {
                    crate::Error::Runtime(format!("Invalid cols in resize: '{}'", parts[0]))
                })?;
                let rows: u32 = parts[1].trim().parse().map_err(|_| {
                    crate::Error::Runtime(format!("Invalid rows in resize: '{}'", parts[1]))
                })?;
                Ok(MockEvent::Resize(cols, rows))
            }
            EventAction::Marker => {
                // Markers don't produce a MockEvent; they're used for expectations.
                // Emit as AppendOutput with a marker prefix so tests can detect it.
                Ok(MockEvent::AppendOutput(format!("[MARKER:{}]", event.name)))
            }
        }
    }

    /// Execute all scenario events on a MockWezterm up to `elapsed` time.
    ///
    /// Returns the number of events executed.
    pub async fn execute_until(&self, mock: &MockWezterm, elapsed: Duration) -> Result<usize> {
        let mut count = 0;
        for event in &self.events {
            if event.at > elapsed {
                break;
            }
            let mock_event = Self::to_mock_event(event)?;
            mock.inject(event.pane, mock_event).await?;
            count += 1;
        }
        Ok(count)
    }

    /// Execute all events in the scenario.
    pub async fn execute_all(&self, mock: &MockWezterm) -> Result<usize> {
        self.execute_until(mock, self.duration).await
    }
}

// ---------------------------------------------------------------------------
// Tutorial Sandbox
// ---------------------------------------------------------------------------

/// A sandboxed simulation environment for tutorial exercises.
///
/// Wraps a [`MockWezterm`] with a pre-configured [`Scenario`] and adds
/// tutorial-specific features: visual indicators, command logging, hints,
/// and exercise-triggered events.
pub struct TutorialSandbox {
    /// The underlying mock terminal.
    mock: MockWezterm,
    /// Active scenario (if loaded).
    scenario: Option<Scenario>,
    /// Commands executed in the sandbox (for progress feedback).
    command_log: Vec<SandboxCommand>,
    /// Whether to prefix output with `[SANDBOX]`.
    show_indicator: bool,
}

/// A command executed within the sandbox.
#[derive(Debug, Clone, Serialize)]
pub struct SandboxCommand {
    /// The command string as entered.
    pub command: String,
    /// Timestamp of execution.
    pub timestamp_ms: u64,
    /// Which exercise was active (if any).
    pub exercise_id: Option<String>,
}

impl TutorialSandbox {
    /// Create a new sandbox with default mock panes for the tutorial.
    pub async fn new() -> Self {
        let mock = MockWezterm::new();
        let scenario = Self::default_scenario();

        if let Err(e) = scenario.setup(&mock).await {
            tracing::warn!("Failed to set up tutorial sandbox scenario: {e}");
        }

        Self {
            mock,
            scenario: Some(scenario),
            command_log: Vec::new(),
            show_indicator: true,
        }
    }

    /// Create a sandbox with a custom scenario.
    pub async fn with_scenario(scenario: Scenario) -> Result<Self> {
        let mock = MockWezterm::new();
        scenario.setup(&mock).await?;

        Ok(Self {
            mock,
            scenario: Some(scenario),
            command_log: Vec::new(),
            show_indicator: true,
        })
    }

    /// Create an empty sandbox with no pre-configured panes.
    pub fn empty() -> Self {
        Self {
            mock: MockWezterm::new(),
            scenario: None,
            command_log: Vec::new(),
            show_indicator: true,
        }
    }

    /// Access the underlying mock terminal.
    pub fn mock(&self) -> &MockWezterm {
        &self.mock
    }

    /// Log a command execution within the sandbox.
    pub fn log_command(&mut self, command: &str, exercise_id: Option<&str>) {
        self.command_log.push(SandboxCommand {
            command: command.to_string(),
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            exercise_id: exercise_id.map(|s| s.to_string()),
        });
    }

    /// Get all commands logged so far.
    pub fn command_log(&self) -> &[SandboxCommand] {
        &self.command_log
    }

    /// Format output with the sandbox indicator.
    pub fn format_output(&self, text: &str) -> String {
        if self.show_indicator {
            format!("[SANDBOX] {text}")
        } else {
            text.to_string()
        }
    }

    /// Enable or disable the `[SANDBOX]` prefix.
    pub fn set_show_indicator(&mut self, show: bool) {
        self.show_indicator = show;
    }

    /// Inject exercise-triggered events into the sandbox.
    ///
    /// This fires all events in the scenario that haven't already been
    /// injected, simulating activity for the current exercise.
    pub async fn trigger_exercise_events(&self) -> Result<usize> {
        match &self.scenario {
            Some(s) => s.execute_all(&self.mock).await,
            None => Ok(0),
        }
    }

    /// Check an expectation against the current sandbox state.
    pub async fn check_expectation(&self, kind: &ExpectationKind) -> bool {
        use crate::wezterm::WeztermInterface;

        match kind {
            ExpectationKind::Contains { pane, text } => {
                if let Ok(content) = self.mock.get_text(*pane, false).await {
                    content.contains(text)
                } else {
                    false
                }
            }
            // Event/Workflow expectations need runtime integration
            _ => false,
        }
    }

    /// Check all expectations from the loaded scenario.
    /// Returns (passed, failed, skipped) counts.
    pub async fn check_all_expectations(&self) -> (usize, usize, usize) {
        let expectations = match &self.scenario {
            Some(s) => &s.expectations,
            None => return (0, 0, 0),
        };

        let mut pass = 0;
        let mut fail = 0;
        let mut skip = 0;

        for exp in expectations {
            match &exp.kind {
                ExpectationKind::Contains { .. } => {
                    if self.check_expectation(&exp.kind).await {
                        pass += 1;
                    } else {
                        fail += 1;
                    }
                }
                _ => skip += 1,
            }
        }

        (pass, fail, skip)
    }

    /// Build the default tutorial sandbox scenario.
    fn default_scenario() -> Scenario {
        Scenario {
            name: "tutorial_sandbox".to_string(),
            description: "Pre-configured environment for wa learn exercises".to_string(),
            duration: Duration::from_secs(300),
            panes: vec![
                ScenarioPane {
                    id: 0,
                    title: "Local Shell".to_string(),
                    domain: "local".to_string(),
                    cwd: "/home/user/projects".to_string(),
                    cols: 80,
                    rows: 24,
                    initial_content: "$ ".to_string(),
                },
                ScenarioPane {
                    id: 1,
                    title: "Codex Agent".to_string(),
                    domain: "local".to_string(),
                    cwd: "/home/user/projects".to_string(),
                    cols: 80,
                    rows: 24,
                    initial_content:
                        "codex> Ready to help with your project.\nWhat would you like to work on?\n"
                            .to_string(),
                },
                ScenarioPane {
                    id: 2,
                    title: "Claude Code".to_string(),
                    domain: "local".to_string(),
                    cwd: "/home/user/projects".to_string(),
                    cols: 80,
                    rows: 24,
                    initial_content: "claude> Analyzing your codebase...\n".to_string(),
                },
            ],
            events: vec![
                ScenarioEvent {
                    at: Duration::from_secs(5),
                    pane: 1,
                    action: EventAction::Append,
                    content: "\n[Usage Warning]\nApproaching daily usage limit.\n".to_string(),
                    name: String::new(),
                    comment: Some("Triggers usage detection exercise".to_string()),
                },
                ScenarioEvent {
                    at: Duration::from_secs(10),
                    pane: 2,
                    action: EventAction::Append,
                    content:
                        "\n[Context Compaction]\nContext window approaching limit. Summarizing...\n"
                            .to_string(),
                    name: String::new(),
                    comment: Some("Triggers compaction detection exercise".to_string()),
                },
            ],
            expectations: vec![
                Expectation {
                    kind: ExpectationKind::Contains {
                        pane: 1,
                        text: "Usage Warning".to_string(),
                    },
                },
                Expectation {
                    kind: ExpectationKind::Contains {
                        pane: 2,
                        text: "Context Compaction".to_string(),
                    },
                },
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Duration deserialization
// ---------------------------------------------------------------------------

fn deserialize_duration<'de, D>(deserializer: D) -> std::result::Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    parse_duration(&s).map_err(serde::de::Error::custom)
}

/// Parse a duration string like "30s", "2m", "1m30s", "1h".
fn parse_duration(s: &str) -> std::result::Result<Duration, String> {
    let s = s.trim();
    let mut total_ms: u64 = 0;
    let mut num_buf = String::new();

    for ch in s.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num_buf.push(ch);
        } else {
            let val: f64 = num_buf
                .parse()
                .map_err(|_| format!("Invalid number in duration: '{num_buf}'"))?;
            num_buf.clear();
            match ch {
                'h' => total_ms += (val * 3_600_000.0) as u64,
                'm' => total_ms += (val * 60_000.0) as u64,
                's' => total_ms += (val * 1_000.0) as u64,
                _ => return Err(format!("Unknown duration unit '{ch}' in '{s}'")),
            }
        }
    }

    if !num_buf.is_empty() {
        let val: f64 = num_buf
            .parse()
            .map_err(|_| format!("Invalid duration: '{s}'"))?;
        total_ms += (val * 1_000.0) as u64;
    }

    Ok(Duration::from_millis(total_ms))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wezterm::WeztermInterface;

    const BASIC_SCENARIO: &str = r#"
name: basic_test
description: "A simple test scenario"
duration: "10s"
panes:
  - id: 0
    title: "Main"
    initial_content: "$ "
events:
  - at: "1s"
    pane: 0
    action: append
    content: "hello world\n"
  - at: "3s"
    pane: 0
    action: append
    content: "done\n"
expectations:
  - contains:
      pane: 0
      text: "hello world"
"#;

    #[test]
    fn parse_basic_scenario() {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        assert_eq!(scenario.name, "basic_test");
        assert_eq!(scenario.duration, Duration::from_secs(10));
        assert_eq!(scenario.panes.len(), 1);
        assert_eq!(scenario.panes[0].id, 0);
        assert_eq!(scenario.panes[0].title, "Main");
        assert_eq!(scenario.events.len(), 2);
        assert_eq!(scenario.events[0].at, Duration::from_secs(1));
        assert_eq!(scenario.events[1].at, Duration::from_secs(3));
    }

    #[test]
    fn parse_multi_pane_scenario() {
        let yaml = r#"
name: multi_pane
description: "Two panes"
duration: "5s"
panes:
  - id: 0
    title: "Left"
  - id: 1
    title: "Right"
    cols: 120
    rows: 40
events:
  - at: "1s"
    pane: 0
    action: append
    content: "left output"
  - at: "2s"
    pane: 1
    action: append
    content: "right output"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        assert_eq!(scenario.panes.len(), 2);
        assert_eq!(scenario.panes[1].cols, 120);
        assert_eq!(scenario.panes[1].rows, 40);
    }

    #[test]
    fn validate_duplicate_pane_ids() {
        let yaml = r#"
name: bad_scenario
duration: "5s"
panes:
  - id: 0
    title: "Pane A"
  - id: 0
    title: "Pane B"
events: []
"#;
        let result = Scenario::from_yaml(yaml);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("Duplicate pane ID"));
    }

    #[test]
    fn validate_unknown_pane_ref() {
        let yaml = r#"
name: bad_ref
duration: "5s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 99
    action: append
    content: "oops"
"#;
        let result = Scenario::from_yaml(yaml);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("unknown pane 99"));
    }

    #[test]
    fn validate_out_of_order_events() {
        let yaml = r#"
name: bad_order
duration: "5s"
panes:
  - id: 0
events:
  - at: "3s"
    pane: 0
    action: append
    content: "second"
  - at: "1s"
    pane: 0
    action: append
    content: "first"
"#;
        let result = Scenario::from_yaml(yaml);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("out of order"));
    }

    #[test]
    fn parse_all_event_actions() {
        let yaml = r#"
name: all_actions
duration: "10s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: append
    content: "text"
  - at: "2s"
    pane: 0
    action: clear
  - at: "3s"
    pane: 0
    action: set_title
    content: "New Title"
  - at: "4s"
    pane: 0
    action: resize
    content: "120x40"
  - at: "5s"
    pane: 0
    action: marker
    name: checkpoint
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        assert_eq!(scenario.events.len(), 5);
        assert_eq!(scenario.events[0].action, EventAction::Append);
        assert_eq!(scenario.events[1].action, EventAction::Clear);
        assert_eq!(scenario.events[2].action, EventAction::SetTitle);
        assert_eq!(scenario.events[3].action, EventAction::Resize);
        assert_eq!(scenario.events[4].action, EventAction::Marker);
    }

    #[test]
    fn to_mock_event_append() {
        let event = ScenarioEvent {
            at: Duration::from_secs(1),
            pane: 0,
            action: EventAction::Append,
            content: "hello".to_string(),
            name: String::new(),
            comment: None,
        };
        let mock_event = Scenario::to_mock_event(&event).unwrap();
        assert!(matches!(mock_event, MockEvent::AppendOutput(ref s) if s == "hello"));
    }

    #[test]
    fn to_mock_event_resize() {
        let event = ScenarioEvent {
            at: Duration::from_secs(1),
            pane: 0,
            action: EventAction::Resize,
            content: "120x40".to_string(),
            name: String::new(),
            comment: None,
        };
        let mock_event = Scenario::to_mock_event(&event).unwrap();
        assert!(matches!(mock_event, MockEvent::Resize(120, 40)));
    }

    #[test]
    fn to_mock_event_resize_invalid() {
        let event = ScenarioEvent {
            at: Duration::from_secs(1),
            pane: 0,
            action: EventAction::Resize,
            content: "bad".to_string(),
            name: String::new(),
            comment: None,
        };
        assert!(Scenario::to_mock_event(&event).is_err());
    }

    #[tokio::test]
    async fn setup_creates_panes() {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        assert_eq!(mock.pane_count().await, 1);
        let state = mock.pane_state(0).await.unwrap();
        assert_eq!(state.title, "Main");
        assert_eq!(state.content, "$ ");
    }

    #[tokio::test]
    async fn execute_all_injects_events() {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let count = scenario.execute_all(&mock).await.unwrap();
        assert_eq!(count, 2);

        let text = mock.get_text(0, false).await.unwrap();
        assert!(text.contains("hello world"));
        assert!(text.contains("done"));
    }

    #[tokio::test]
    async fn execute_until_partial() {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        // Only execute events up to 2s (only the first event at 1s fires)
        let count = scenario
            .execute_until(&mock, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(count, 1);

        let text = mock.get_text(0, false).await.unwrap();
        assert!(text.contains("hello world"));
        assert!(!text.contains("done"));
    }

    #[tokio::test]
    async fn scenario_with_clear() {
        let yaml = r#"
name: clear_test
duration: "5s"
panes:
  - id: 0
    initial_content: "old content"
events:
  - at: "1s"
    pane: 0
    action: clear
  - at: "2s"
    pane: 0
    action: append
    content: "new content"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();
        scenario.execute_all(&mock).await.unwrap();

        let text = mock.get_text(0, false).await.unwrap();
        assert!(!text.contains("old content"));
        assert!(text.contains("new content"));
    }

    #[tokio::test]
    async fn scenario_with_resize_and_title() {
        let yaml = r#"
name: resize_title
duration: "5s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: resize
    content: "120x40"
  - at: "2s"
    pane: 0
    action: set_title
    content: "Updated Title"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();
        scenario.execute_all(&mock).await.unwrap();

        let state = mock.pane_state(0).await.unwrap();
        assert_eq!(state.cols, 120);
        assert_eq!(state.rows, 40);
        assert_eq!(state.title, "Updated Title");
    }

    #[test]
    fn parse_duration_values() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("0.5s").unwrap(), Duration::from_millis(500));
    }

    #[test]
    fn parse_expectations() {
        let yaml = r#"
name: with_expectations
duration: "10s"
panes:
  - id: 0
events: []
expectations:
  - event:
      event: usage_limit
      detected_at: "~8s"
  - workflow:
      workflow: handle_usage_limits
      started_at: "~9s"
  - contains:
      pane: 0
      text: "hello"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        assert_eq!(scenario.expectations.len(), 3);
    }

    #[test]
    fn empty_scenario_is_valid() {
        let yaml = r#"
name: empty
duration: "1s"
panes: []
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        assert!(scenario.panes.is_empty());
        assert!(scenario.events.is_empty());
    }

    #[test]
    fn scenario_defaults() {
        let yaml = r#"
name: defaults
duration: "5s"
panes:
  - id: 0
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let pane = &scenario.panes[0];
        assert_eq!(pane.title, "pane");
        assert_eq!(pane.domain, "local");
        assert_eq!(pane.cwd, "/home/user");
        assert_eq!(pane.cols, 80);
        assert_eq!(pane.rows, 24);
        assert!(pane.initial_content.is_empty());
    }

    #[tokio::test]
    async fn multi_pane_execution() {
        let yaml = r#"
name: multi_exec
duration: "5s"
panes:
  - id: 0
    title: "Agent A"
  - id: 1
    title: "Agent B"
events:
  - at: "1s"
    pane: 0
    action: append
    content: "output-a"
  - at: "2s"
    pane: 1
    action: append
    content: "output-b"
  - at: "3s"
    pane: 0
    action: append
    content: " more-a"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();
        let count = scenario.execute_all(&mock).await.unwrap();
        assert_eq!(count, 3);

        let t0 = mock.get_text(0, false).await.unwrap();
        let t1 = mock.get_text(1, false).await.unwrap();
        assert!(t0.contains("output-a"));
        assert!(t0.contains("more-a"));
        assert!(t1.contains("output-b"));
        assert!(!t1.contains("output-a"));
    }

    #[tokio::test]
    async fn marker_event_injects_marker_text() {
        let yaml = r#"
name: marker_test
duration: "5s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: marker
    name: checkpoint_1
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();
        scenario.execute_all(&mock).await.unwrap();

        let text = mock.get_text(0, false).await.unwrap();
        assert!(text.contains("[MARKER:checkpoint_1]"));
    }

    #[tokio::test]
    async fn contains_expectation_passes() {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();
        scenario.execute_all(&mock).await.unwrap();

        // Verify the expectation programmatically
        assert_eq!(scenario.expectations.len(), 1);
        match &scenario.expectations[0].kind {
            ExpectationKind::Contains { pane, text } => {
                let content = mock.get_text(*pane, false).await.unwrap();
                assert!(content.contains(text));
            }
            _ => panic!("Expected Contains expectation"),
        }
    }

    #[test]
    fn comments_are_ignored() {
        let yaml = r#"
name: with_comments
duration: "5s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: append
    content: "hello"
    comment: "This is a test event"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        assert_eq!(
            scenario.events[0].comment.as_deref(),
            Some("This is a test event")
        );
    }

    #[test]
    fn to_mock_event_clear() {
        let event = ScenarioEvent {
            at: Duration::from_secs(1),
            pane: 0,
            action: EventAction::Clear,
            content: String::new(),
            name: String::new(),
            comment: None,
        };
        let mock_event = Scenario::to_mock_event(&event).unwrap();
        assert!(matches!(mock_event, MockEvent::ClearScreen));
    }

    #[test]
    fn to_mock_event_set_title() {
        let event = ScenarioEvent {
            at: Duration::from_secs(1),
            pane: 0,
            action: EventAction::SetTitle,
            content: "My Title".to_string(),
            name: String::new(),
            comment: None,
        };
        let mock_event = Scenario::to_mock_event(&event).unwrap();
        assert!(matches!(mock_event, MockEvent::SetTitle(ref s) if s == "My Title"));
    }

    #[test]
    fn to_mock_event_marker() {
        let event = ScenarioEvent {
            at: Duration::from_secs(1),
            pane: 0,
            action: EventAction::Marker,
            content: String::new(),
            name: "my_marker".to_string(),
            comment: None,
        };
        let mock_event = Scenario::to_mock_event(&event).unwrap();
        assert!(matches!(mock_event, MockEvent::AppendOutput(ref s) if s.contains("my_marker")));
    }

    #[test]
    fn duration_parse_edge_cases() {
        // Pure seconds as float
        assert_eq!(parse_duration("0.1s").unwrap(), Duration::from_millis(100));
        // Hour + minute
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        // All units
        assert_eq!(parse_duration("1h1m1s").unwrap(), Duration::from_secs(3661));
    }

    #[test]
    fn duration_parse_bad_unit() {
        assert!(parse_duration("5x").is_err());
    }

    #[test]
    fn duration_parse_empty_number() {
        assert!(parse_duration("s").is_err());
    }

    #[tokio::test]
    async fn execute_until_zero_runs_nothing() {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let count = scenario
            .execute_until(&mock, Duration::from_millis(0))
            .await
            .unwrap();
        assert_eq!(count, 0);

        let text = mock.get_text(0, false).await.unwrap();
        assert_eq!(text, "$ ");
    }

    #[test]
    fn scenario_round_trip_yaml() {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let serialized = serde_yaml::to_string(&scenario).unwrap();
        // Verify it can be deserialized back (not a perfect round-trip due to Duration,
        // but the key fields survive)
        assert!(serialized.contains("basic_test"));
        assert!(serialized.contains("hello world"));
    }

    #[tokio::test]
    async fn scenario_load_from_temp_file() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "{}", BASIC_SCENARIO).unwrap();
        drop(f);

        let scenario = Scenario::load(&path).unwrap();
        assert_eq!(scenario.name, "basic_test");
        assert_eq!(scenario.events.len(), 2);
    }

    #[test]
    fn scenario_load_nonexistent_file() {
        let result = Scenario::load(std::path::Path::new("/nonexistent/scenario.yaml"));
        assert!(result.is_err());
    }

    #[test]
    fn scenario_invalid_yaml_returns_error() {
        let yaml = "this is not valid yaml: [[[";
        assert!(Scenario::from_yaml(yaml).is_err());
    }

    #[test]
    fn scenario_missing_name_field() {
        let yaml = r#"
duration: "5s"
panes: []
events: []
"#;
        assert!(Scenario::from_yaml(yaml).is_err());
    }

    #[test]
    fn scenario_missing_duration_field() {
        let yaml = r"
name: no_duration
panes: []
events: []
";
        assert!(Scenario::from_yaml(yaml).is_err());
    }

    // -----------------------------------------------------------------------
    // TutorialSandbox tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn sandbox_creates_default_panes() {
        let sandbox = TutorialSandbox::new().await;
        assert_eq!(sandbox.mock().pane_count().await, 3);

        let p0 = sandbox.mock().pane_state(0).await.unwrap();
        assert_eq!(p0.title, "Local Shell");
        let p1 = sandbox.mock().pane_state(1).await.unwrap();
        assert_eq!(p1.title, "Codex Agent");
        let p2 = sandbox.mock().pane_state(2).await.unwrap();
        assert_eq!(p2.title, "Claude Code");
    }

    #[tokio::test]
    async fn sandbox_initial_content() {
        let sandbox = TutorialSandbox::new().await;

        let t0 = sandbox.mock().get_text(0, false).await.unwrap();
        assert_eq!(t0, "$ ");
        let t1 = sandbox.mock().get_text(1, false).await.unwrap();
        assert!(t1.contains("codex>"));
    }

    #[tokio::test]
    async fn sandbox_format_output_with_indicator() {
        let sandbox = TutorialSandbox::new().await;
        assert_eq!(sandbox.format_output("hello"), "[SANDBOX] hello");
    }

    #[tokio::test]
    async fn sandbox_format_output_without_indicator() {
        let mut sandbox = TutorialSandbox::new().await;
        sandbox.set_show_indicator(false);
        assert_eq!(sandbox.format_output("hello"), "hello");
    }

    #[tokio::test]
    async fn sandbox_command_logging() {
        let mut sandbox = TutorialSandbox::new().await;
        assert!(sandbox.command_log().is_empty());

        sandbox.log_command("wa status", Some("basics.1"));
        sandbox.log_command("wa list", None);

        assert_eq!(sandbox.command_log().len(), 2);
        assert_eq!(sandbox.command_log()[0].command, "wa status");
        assert_eq!(
            sandbox.command_log()[0].exercise_id.as_deref(),
            Some("basics.1")
        );
        assert_eq!(sandbox.command_log()[1].command, "wa list");
        assert!(sandbox.command_log()[1].exercise_id.is_none());
    }

    #[tokio::test]
    async fn sandbox_trigger_events() {
        let sandbox = TutorialSandbox::new().await;
        let count = sandbox.trigger_exercise_events().await.unwrap();
        assert_eq!(count, 2);

        let t1 = sandbox.mock().get_text(1, false).await.unwrap();
        assert!(t1.contains("Usage Warning"));
        let t2 = sandbox.mock().get_text(2, false).await.unwrap();
        assert!(t2.contains("Context Compaction"));
    }

    #[tokio::test]
    async fn sandbox_check_expectations_after_events() {
        let sandbox = TutorialSandbox::new().await;
        sandbox.trigger_exercise_events().await.unwrap();

        let (pass, fail, skip) = sandbox.check_all_expectations().await;
        assert_eq!(pass, 2);
        assert_eq!(fail, 0);
        assert_eq!(skip, 0);
    }

    #[tokio::test]
    async fn sandbox_check_expectations_before_events() {
        let sandbox = TutorialSandbox::new().await;
        // Don't trigger events — expectations should fail
        let (pass, fail, skip) = sandbox.check_all_expectations().await;
        assert_eq!(pass, 0);
        assert_eq!(fail, 2);
        assert_eq!(skip, 0);
    }

    #[tokio::test]
    async fn sandbox_with_custom_scenario() {
        let yaml = r#"
name: custom_sandbox
duration: "5s"
panes:
  - id: 0
    title: "Custom"
    initial_content: "custom> "
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let sandbox = TutorialSandbox::with_scenario(scenario).await.unwrap();

        assert_eq!(sandbox.mock().pane_count().await, 1);
        let text = sandbox.mock().get_text(0, false).await.unwrap();
        assert_eq!(text, "custom> ");
    }

    #[tokio::test]
    async fn sandbox_empty_has_no_panes() {
        let sandbox = TutorialSandbox::empty();
        assert_eq!(sandbox.mock().pane_count().await, 0);
    }

    #[tokio::test]
    async fn sandbox_empty_trigger_events_returns_zero() {
        let sandbox = TutorialSandbox::empty();
        let count = sandbox.trigger_exercise_events().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn sandbox_empty_check_expectations() {
        let sandbox = TutorialSandbox::empty();
        let (pass, fail, skip) = sandbox.check_all_expectations().await;
        assert_eq!(pass, 0);
        assert_eq!(fail, 0);
        assert_eq!(skip, 0);
    }
}
