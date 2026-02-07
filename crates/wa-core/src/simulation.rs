//! Simulation scenario system for testing and demos.
//!
//! Defines declarative YAML scenarios that can be applied to a
//! [`MockWezterm`](crate::wezterm::MockWezterm) for reproducible testing
//! and interactive demonstrations.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::wezterm::{MockEvent, MockPane, MockWezterm};
use crate::Result;

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
        let scenario: Scenario = serde_yaml::from_str(yaml).map_err(|e| {
            crate::Error::Runtime(format!("Failed to parse scenario YAML: {e}"))
        })?;
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
                Ok(MockEvent::AppendOutput(format!(
                    "[MARKER:{}]",
                    event.name
                )))
            }
        }
    }

    /// Execute all scenario events on a MockWezterm up to `elapsed` time.
    ///
    /// Returns the number of events executed.
    pub async fn execute_until(
        &self,
        mock: &MockWezterm,
        elapsed: Duration,
    ) -> Result<usize> {
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
        assert_eq!(
            parse_duration("1m30s").unwrap(),
            Duration::from_secs(90)
        );
        assert_eq!(
            parse_duration("1h").unwrap(),
            Duration::from_secs(3600)
        );
        assert_eq!(
            parse_duration("0.5s").unwrap(),
            Duration::from_millis(500)
        );
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
}
