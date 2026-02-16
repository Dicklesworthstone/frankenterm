//! Simulation scenario system for testing and demos.
//!
//! Defines declarative YAML scenarios that can be applied to a
//! [`MockWezterm`](crate::wezterm::MockWezterm) for reproducible testing
//! and interactive demonstrations.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// Reproducibility metadata for deterministic baseline comparisons.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
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
    /// Window ID for layout-scale scenarios.
    #[serde(default = "default_window_id")]
    pub window_id: u64,
    /// Tab ID for layout-scale scenarios.
    #[serde(default = "default_tab_id")]
    pub tab_id: u64,
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
fn default_window_id() -> u64 {
    0
}
fn default_tab_id() -> u64 {
    0
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
    /// Record a font-size transition marker. Uses `content` as the size token.
    SetFontSize,
    /// Deterministically synthesize scrollback text. Uses `content` as "LINES" or "LINESxWIDTH".
    GenerateScrollback,
    /// Simulate interactive typing input. Uses `content` as typed payload.
    Typing,
    /// Simulate bracketed paste input. Uses `content` as pasted payload.
    Paste,
    /// Simulate mouse interaction. Uses `name` or `content` as interaction token.
    Mouse,
    /// Insert a named marker (for expectations).
    Marker,
}

impl EventAction {
    /// Canonical snake-case action string for metrics/events.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Clear => "clear",
            Self::SetTitle => "set_title",
            Self::Resize => "resize",
            Self::SetFontSize => "set_font_size",
            Self::GenerateScrollback => "generate_scrollback",
            Self::Typing => "typing",
            Self::Paste => "paste",
            Self::Mouse => "mouse",
            Self::Marker => "marker",
        }
    }

    /// Whether this action participates in timeline attribution.
    #[must_use]
    pub const fn is_resize_timeline_action(&self) -> bool {
        matches!(
            self,
            Self::Resize
                | Self::SetFontSize
                | Self::GenerateScrollback
                | Self::Typing
                | Self::Paste
                | Self::Mouse
        )
    }
}

/// Ordered execution stages for resize/reflow timeline attribution.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ResizeTimelineStage {
    /// Event intent was received/selected.
    InputIntent,
    /// Event spent time queued before processing.
    SchedulerQueueing,
    /// Logical reflow and semantic action parsing.
    LogicalReflow,
    /// Render-preparation work before presentation.
    RenderPrep,
    /// Final presentation/injection stage.
    Presentation,
}

impl ResizeTimelineStage {
    /// Canonical stage order for per-event probes and summaries.
    pub const ALL: [Self; 5] = [
        Self::InputIntent,
        Self::SchedulerQueueing,
        Self::LogicalReflow,
        Self::RenderPrep,
        Self::Presentation,
    ];

    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::InputIntent => "input_intent",
            Self::SchedulerQueueing => "scheduler_queueing",
            Self::LogicalReflow => "logical_reflow",
            Self::RenderPrep => "render_prep",
            Self::Presentation => "presentation",
        }
    }
}

/// Queue depth metrics captured for queueing stages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResizeQueueMetrics {
    /// Pending resize-class events before current event is dequeued.
    pub depth_before: u64,
    /// Pending resize-class events after current event is dequeued.
    pub depth_after: u64,
}

/// Atlas invalidation policy selected for a font-size transition.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FontAtlasCachePolicy {
    /// Reuse existing atlas entries where possible; minimal invalidation.
    ReuseHotAtlas,
    /// Invalidate only affected regions and keep hot glyph coverage.
    SelectiveInvalidate,
    /// Full atlas rebuild required for correctness.
    FullRebuild,
}

/// Structured render-prep metrics for font-size transitions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FontRenderPrepMetrics {
    /// Atlas policy selected for this transition.
    pub atlas_cache_policy: FontAtlasCachePolicy,
    /// Whether shader warmup was performed before commit.
    pub shader_warmup: bool,
    /// Glyphs served from cache for this event.
    pub cache_hit_glyphs: u32,
    /// Glyphs rebuilt synchronously in the current frame.
    pub glyphs_rebuilt_now: u32,
    /// Glyphs deferred to follow-up staging batches.
    pub deferred_glyphs: u32,
    /// Total staging batches implied by this transition.
    pub staged_batches_total: u32,
    /// Number of batches deferred after the current frame.
    pub staged_batches_deferred: u32,
}

/// One stage timing sample for a single resize timeline event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResizeTimelineStageSample {
    /// Pipeline stage identifier.
    pub stage: ResizeTimelineStage,
    /// Stage start offset from event dispatch (nanoseconds).
    pub start_offset_ns: u64,
    /// Stage duration (nanoseconds).
    pub duration_ns: u64,
    /// Queue depth attribution (present for scheduler stage).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_metrics: Option<ResizeQueueMetrics>,
    /// Structured render-prep metrics (present for font-size transitions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_prep_metrics: Option<FontRenderPrepMetrics>,
}

/// Per-event timeline attribution for resize-class actions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResizeTimelineEvent {
    /// 0-based event index in scenario event list.
    pub event_index: usize,
    /// Deterministic correlation ID for this resize transaction.
    pub resize_transaction_id: String,
    /// Target pane ID.
    pub pane_id: u64,
    /// Target tab ID for the pane at execution time.
    pub tab_id: u64,
    /// Monotonic sequence number within scenario execution.
    pub sequence_no: u64,
    /// Action executed.
    pub action: EventAction,
    /// Scheduler decision label used for this event.
    pub scheduler_decision: String,
    /// Synthetic frame identifier for render/present attribution.
    pub frame_id: u64,
    /// Scenario-level test case identifier.
    pub test_case_id: String,
    /// Queue wait stage duration in milliseconds.
    pub queue_wait_ms: u64,
    /// Logical reflow stage duration in milliseconds.
    pub reflow_ms: u64,
    /// Render prep stage duration in milliseconds.
    pub render_ms: u64,
    /// Presentation stage duration in milliseconds.
    pub present_ms: u64,
    /// Scheduled scenario timestamp offset (nanoseconds).
    pub scheduled_at_ns: u64,
    /// Actual dispatch offset relative to scenario execution start (nanoseconds).
    pub dispatch_offset_ns: u64,
    /// Total wall duration for this event probe (nanoseconds).
    pub total_duration_ns: u64,
    /// Ordered stage probes.
    pub stages: Vec<ResizeTimelineStageSample>,
}

/// Flamegraph-friendly sample row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResizeTimelineFlameSample {
    /// Collapsed stack label (`scenario;action;stage`).
    pub stack: String,
    /// Sample value in nanoseconds.
    pub duration_ns: u64,
    /// Source event index.
    pub event_index: usize,
    /// Source pane ID.
    pub pane_id: u64,
}

/// Aggregate stats per resize timeline stage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResizeTimelineStageSummary {
    /// Stage identifier.
    pub stage: ResizeTimelineStage,
    /// Number of samples contributing to this stage.
    pub samples: usize,
    /// Sum of all sample durations (nanoseconds).
    pub total_duration_ns: u64,
    /// Arithmetic mean duration (nanoseconds).
    pub avg_duration_ns: f64,
    /// p50 duration (nanoseconds).
    pub p50_duration_ns: u64,
    /// p95 duration (nanoseconds).
    pub p95_duration_ns: u64,
    /// p99 duration (nanoseconds).
    pub p99_duration_ns: u64,
    /// Maximum observed duration (nanoseconds).
    pub max_duration_ns: u64,
}

/// Resize timeline artifact for one scenario execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResizeTimeline {
    /// Scenario name.
    pub scenario: String,
    /// Reproducibility key from scenario metadata.
    pub reproducibility_key: String,
    /// Capture timestamp in epoch ms.
    pub captured_at_ms: u64,
    /// Resize-class events executed and probed.
    pub executed_resize_events: usize,
    /// Per-event stage probes.
    pub events: Vec<ResizeTimelineEvent>,
}

impl ResizeTimeline {
    /// Build flamegraph-ready rows from stage samples.
    #[must_use]
    pub fn flame_samples(&self) -> Vec<ResizeTimelineFlameSample> {
        let mut rows = Vec::new();
        for event in &self.events {
            for stage in &event.stages {
                rows.push(ResizeTimelineFlameSample {
                    stack: format!(
                        "{};{};{}",
                        self.scenario,
                        event.action.as_str(),
                        stage.stage.as_str()
                    ),
                    duration_ns: stage.duration_ns,
                    event_index: event.event_index,
                    pane_id: event.pane_id,
                });
            }
        }
        rows
    }

    /// Per-stage summary for attribution and regression triage.
    #[must_use]
    pub fn stage_summary(&self) -> Vec<ResizeTimelineStageSummary> {
        let mut per_stage: BTreeMap<ResizeTimelineStage, Vec<u64>> = BTreeMap::new();
        for event in &self.events {
            for stage in &event.stages {
                per_stage
                    .entry(stage.stage)
                    .or_default()
                    .push(stage.duration_ns);
            }
        }

        ResizeTimelineStage::ALL
            .iter()
            .copied()
            .map(|stage| {
                let mut samples = per_stage.remove(&stage).unwrap_or_default();
                samples.sort_unstable();
                let count = samples.len();
                let total = samples.iter().fold(0u64, |acc, v| acc.saturating_add(*v));
                let avg = if count == 0 {
                    0.0
                } else {
                    total as f64 / count as f64
                };
                let max = samples.last().copied().unwrap_or(0);
                let percentile = |pct: usize| -> u64 {
                    if count == 0 {
                        0
                    } else {
                        let idx = ((count - 1) * pct) / 100;
                        samples[idx]
                    }
                };
                let p50 = percentile(50);
                let p95 = percentile(95);
                let p99 = percentile(99);
                ResizeTimelineStageSummary {
                    stage,
                    samples: count,
                    total_duration_ns: total,
                    avg_duration_ns: avg,
                    p50_duration_ns: p50,
                    p95_duration_ns: p95,
                    p99_duration_ns: p99,
                    max_duration_ns: max,
                }
            })
            .collect()
    }
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
        let mut seen_ids = HashSet::new();
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

            match event.action {
                EventAction::Resize => {
                    let _ = parse_resize_spec(&event.content)?;
                }
                EventAction::SetFontSize => {
                    if event.content.trim().is_empty() {
                        return Err(crate::Error::Runtime(format!(
                            "SetFontSize requires non-empty content in scenario '{}'",
                            self.name
                        )));
                    }
                }
                EventAction::GenerateScrollback => {
                    let _ = parse_scrollback_spec(&event.content)?;
                }
                EventAction::Typing | EventAction::Paste => {
                    if event.content.trim().is_empty() {
                        return Err(crate::Error::Runtime(format!(
                            "{:?} requires non-empty content in scenario '{}'",
                            event.action, self.name
                        )));
                    }
                }
                EventAction::Mouse => {
                    if event.name.trim().is_empty() && event.content.trim().is_empty() {
                        return Err(crate::Error::Runtime(format!(
                            "Mouse requires non-empty name or content in scenario '{}'",
                            self.name
                        )));
                    }
                }
                EventAction::Append
                | EventAction::Clear
                | EventAction::SetTitle
                | EventAction::Marker => {}
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

        for (key, value) in &self.metadata {
            if key.trim().is_empty() {
                return Err(crate::Error::Runtime(format!(
                    "Scenario '{}' contains an empty metadata key",
                    self.name
                )));
            }
            if value.trim().is_empty() {
                return Err(crate::Error::Runtime(format!(
                    "Scenario '{}' metadata '{}' has an empty value",
                    self.name, key
                )));
            }
        }

        Ok(())
    }

    /// Stable reproducibility identifier for comparing baseline runs over time.
    #[must_use]
    pub fn reproducibility_key(&self) -> String {
        let suite = self
            .metadata
            .get("suite")
            .map(String::as_str)
            .unwrap_or("ad_hoc");
        let suite_version = self
            .metadata
            .get("suite_version")
            .map(String::as_str)
            .unwrap_or("v1");
        let seed = self.metadata.get("seed").map(String::as_str).unwrap_or("0");
        format!("{suite}:{suite_version}:{}:{seed}", self.name)
    }

    /// Apply scenario panes and initial content to a MockWezterm.
    pub async fn setup(&self, mock: &MockWezterm) -> Result<()> {
        for pane_def in &self.panes {
            let pane = MockPane {
                pane_id: pane_def.id,
                window_id: pane_def.window_id,
                tab_id: pane_def.tab_id,
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
                let (cols, rows) = parse_resize_spec(&event.content)?;
                Ok(MockEvent::Resize(cols, rows))
            }
            EventAction::SetFontSize => Ok(MockEvent::AppendOutput(format!(
                "[FONT_SIZE:{}]",
                event.content.trim()
            ))),
            EventAction::GenerateScrollback => {
                let (lines, width) = parse_scrollback_spec(&event.content)?;
                Ok(MockEvent::AppendOutput(generate_scrollback(lines, width)))
            }
            EventAction::Typing => Ok(MockEvent::AppendOutput(format!(
                "[TYPING:{}]",
                event.content
            ))),
            EventAction::Paste => Ok(MockEvent::AppendOutput(format!("[PASTE:{}]", event.content))),
            EventAction::Mouse => {
                let token = if event.name.trim().is_empty() {
                    event.content.trim()
                } else {
                    event.name.trim()
                };
                Ok(MockEvent::AppendOutput(format!("[MOUSE:{token}]")))
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

    /// Execute events up to `elapsed` and capture stage-level resize timeline probes.
    ///
    /// This emits structured per-event stage timings for resize-class actions
    /// (`resize`, `set_font_size`, `generate_scrollback`) with queue depth
    /// attribution suitable for flamegraph/post-hoc analysis.
    pub async fn execute_until_with_resize_timeline(
        &self,
        mock: &MockWezterm,
        elapsed: Duration,
    ) -> Result<(usize, ResizeTimeline)> {
        let mut count = 0usize;
        let run_started = Instant::now();
        let reproducibility_key = self.reproducibility_key();
        let events_in_window = self.events.iter().take_while(|e| e.at <= elapsed);
        let resize_total = events_in_window
            .clone()
            .filter(|e| e.action.is_resize_timeline_action())
            .count();
        let mut resize_seen = 0usize;
        let mut timeline_events = Vec::new();
        let mut font_pipeline_state: HashMap<u64, FontRenderPipelineState> = HashMap::new();

        for (index, event) in self.events.iter().enumerate() {
            if event.at > elapsed {
                break;
            }

            if !event.action.is_resize_timeline_action() {
                let mock_event = Self::to_mock_event(event)?;
                mock.inject(event.pane, mock_event).await?;
                count += 1;
                continue;
            }

            let event_started = Instant::now();
            let dispatch_offset_ns = duration_ns_u64(run_started.elapsed());
            let scheduled_at_ns = duration_ns_u64(event.at);
            let mut stages = Vec::with_capacity(ResizeTimelineStage::ALL.len());
            let mut offset_ns = 0u64;

            // Stage 1: input intent
            let stage_started = Instant::now();
            let _ = scheduled_at_ns;
            let input_intent_duration_ns = duration_ns_u64(stage_started.elapsed());
            stages.push(ResizeTimelineStageSample {
                stage: ResizeTimelineStage::InputIntent,
                start_offset_ns: offset_ns,
                duration_ns: input_intent_duration_ns,
                queue_metrics: None,
                render_prep_metrics: None,
            });
            offset_ns = offset_ns.saturating_add(input_intent_duration_ns);

            // Stage 2: scheduler queueing
            let depth_before =
                u64::try_from(resize_total.saturating_sub(resize_seen)).unwrap_or(u64::MAX);
            let depth_after = depth_before.saturating_sub(1);
            let stage_started = Instant::now();
            let queue_delay_ns = dispatch_offset_ns.saturating_sub(scheduled_at_ns);
            let scheduler_queue_duration_ns =
                duration_ns_u64(stage_started.elapsed()).saturating_add(queue_delay_ns);
            stages.push(ResizeTimelineStageSample {
                stage: ResizeTimelineStage::SchedulerQueueing,
                start_offset_ns: offset_ns,
                duration_ns: scheduler_queue_duration_ns,
                queue_metrics: Some(ResizeQueueMetrics {
                    depth_before,
                    depth_after,
                }),
                render_prep_metrics: None,
            });
            offset_ns = offset_ns.saturating_add(scheduler_queue_duration_ns);

            // Stage 3: logical reflow
            let stage_started = Instant::now();
            let mock_event = Self::to_mock_event(event)?;
            let logical_reflow_duration_ns = duration_ns_u64(stage_started.elapsed());
            stages.push(ResizeTimelineStageSample {
                stage: ResizeTimelineStage::LogicalReflow,
                start_offset_ns: offset_ns,
                duration_ns: logical_reflow_duration_ns,
                queue_metrics: None,
                render_prep_metrics: None,
            });
            offset_ns = offset_ns.saturating_add(logical_reflow_duration_ns);

            // Stage 4: render prep
            let stage_started = Instant::now();
            let (planned_render_prep_ns, render_prep_metrics) = plan_render_prep(
                &event.action,
                event.pane,
                &event.content,
                &mock_event,
                &mut font_pipeline_state,
            );
            let render_prep_duration_ns = planned_render_prep_ns
                .max(duration_ns_u64(stage_started.elapsed()))
                .max(1);
            stages.push(ResizeTimelineStageSample {
                stage: ResizeTimelineStage::RenderPrep,
                start_offset_ns: offset_ns,
                duration_ns: render_prep_duration_ns,
                queue_metrics: None,
                render_prep_metrics,
            });
            offset_ns = offset_ns.saturating_add(render_prep_duration_ns);

            // Stage 5: presentation
            let stage_started = Instant::now();
            mock.inject(event.pane, mock_event).await?;
            let presentation_duration_ns = duration_ns_u64(stage_started.elapsed());
            stages.push(ResizeTimelineStageSample {
                stage: ResizeTimelineStage::Presentation,
                start_offset_ns: offset_ns,
                duration_ns: presentation_duration_ns,
                queue_metrics: None,
                render_prep_metrics: None,
            });

            let total_duration_ns = duration_ns_u64(event_started.elapsed());
            let sequence_no = u64::try_from(index).unwrap_or(u64::MAX);
            let tab_id = self
                .panes
                .iter()
                .find(|pane| pane.id == event.pane)
                .map_or(0, |pane| pane.tab_id);
            let scheduler_decision = if depth_before > depth_after {
                "dequeue_latest_intent"
            } else {
                "noop"
            }
            .to_string();
            timeline_events.push(ResizeTimelineEvent {
                event_index: index,
                resize_transaction_id: format!("{reproducibility_key}:{index}"),
                pane_id: event.pane,
                tab_id,
                sequence_no,
                action: event.action.clone(),
                scheduler_decision,
                frame_id: sequence_no,
                test_case_id: self.name.clone(),
                queue_wait_ms: ns_to_ms_u64(scheduler_queue_duration_ns),
                reflow_ms: ns_to_ms_u64(logical_reflow_duration_ns),
                render_ms: ns_to_ms_u64(render_prep_duration_ns),
                present_ms: ns_to_ms_u64(presentation_duration_ns),
                scheduled_at_ns,
                dispatch_offset_ns,
                total_duration_ns,
                stages,
            });
            resize_seen += 1;
            count += 1;
        }

        Ok((
            count,
            ResizeTimeline {
                scenario: self.name.clone(),
                reproducibility_key,
                captured_at_ms: epoch_ms_u64(),
                executed_resize_events: timeline_events.len(),
                events: timeline_events,
            },
        ))
    }

    /// Execute all events and capture stage-level resize timeline probes.
    pub async fn execute_all_with_resize_timeline(
        &self,
        mock: &MockWezterm,
    ) -> Result<(usize, ResizeTimeline)> {
        self.execute_until_with_resize_timeline(mock, self.duration)
            .await
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
            metadata: BTreeMap::new(),
            panes: vec![
                ScenarioPane {
                    id: 0,
                    title: "Local Shell".to_string(),
                    domain: "local".to_string(),
                    cwd: "/home/user/projects".to_string(),
                    window_id: 0,
                    tab_id: 0,
                    cols: 80,
                    rows: 24,
                    initial_content: "$ ".to_string(),
                },
                ScenarioPane {
                    id: 1,
                    title: "Codex Agent".to_string(),
                    domain: "local".to_string(),
                    cwd: "/home/user/projects".to_string(),
                    window_id: 0,
                    tab_id: 0,
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
                    window_id: 0,
                    tab_id: 0,
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

fn parse_resize_spec(spec: &str) -> Result<(u32, u32)> {
    let (cols_raw, rows_raw) = spec
        .split_once('x')
        .or_else(|| spec.split_once('X'))
        .ok_or_else(|| {
            crate::Error::Runtime(format!("Resize content must be 'COLSxROWS', got '{spec}'"))
        })?;

    let cols: u32 = cols_raw
        .trim()
        .parse()
        .map_err(|_| crate::Error::Runtime(format!("Invalid cols in resize: '{cols_raw}'")))?;
    let rows: u32 = rows_raw
        .trim()
        .parse()
        .map_err(|_| crate::Error::Runtime(format!("Invalid rows in resize: '{rows_raw}'")))?;

    if cols == 0 || rows == 0 {
        return Err(crate::Error::Runtime(format!(
            "Resize dimensions must be > 0, got '{spec}'"
        )));
    }

    Ok((cols, rows))
}

fn parse_scrollback_spec(spec: &str) -> Result<(usize, usize)> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(crate::Error::Runtime(
            "GenerateScrollback requires non-empty content".to_string(),
        ));
    }

    let (lines_raw, width_raw_opt) =
        if let Some((l, w)) = trimmed.split_once('x').or_else(|| trimmed.split_once('X')) {
            (l.trim(), Some(w.trim()))
        } else {
            (trimmed, None)
        };

    let lines: usize = lines_raw.parse().map_err(|_| {
        crate::Error::Runtime(format!(
            "Invalid line count for GenerateScrollback: '{lines_raw}'"
        ))
    })?;
    if lines == 0 {
        return Err(crate::Error::Runtime(
            "GenerateScrollback line count must be > 0".to_string(),
        ));
    }
    if lines > 250_000 {
        return Err(crate::Error::Runtime(format!(
            "GenerateScrollback line count too large ({lines}); max 250000"
        )));
    }

    let width: usize = match width_raw_opt {
        Some(raw) => raw.parse().map_err(|_| {
            crate::Error::Runtime(format!("Invalid width for GenerateScrollback: '{raw}'"))
        })?,
        None => 96,
    };
    if !(20..=4096).contains(&width) {
        return Err(crate::Error::Runtime(format!(
            "GenerateScrollback width out of range ({width}); expected 20..=4096"
        )));
    }

    Ok((lines, width))
}

fn generate_scrollback(lines: usize, width: usize) -> String {
    const FILLER: &str = "abcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(lines.saturating_mul(width.saturating_add(1)));
    for i in 0..lines {
        let mut line = format!("[scrollback:{i:06}] ");
        while line.len() < width {
            line.push_str(FILLER);
        }
        line.truncate(width);
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[derive(Debug, Clone, Default)]
struct FontRenderPipelineState {
    last_font_scale: Option<f64>,
    cached_glyphs: u32,
    shader_warmed: bool,
}

fn plan_render_prep(
    action: &EventAction,
    pane_id: u64,
    content: &str,
    mock_event: &MockEvent,
    state: &mut HashMap<u64, FontRenderPipelineState>,
) -> (u64, Option<FontRenderPrepMetrics>) {
    if *action != EventAction::SetFontSize {
        let render_hint = match mock_event {
            MockEvent::AppendOutput(text) => text.len(),
            MockEvent::SetTitle(text) => text.len(),
            MockEvent::Resize(cols, rows) => (*cols as usize) + (*rows as usize),
            MockEvent::ClearScreen => 0,
        };
        let hint_u64 = u64::try_from(render_hint).unwrap_or(u64::MAX);
        let duration_ns = 20_000u64.saturating_add(hint_u64.saturating_mul(75));
        return (duration_ns, None);
    }

    let target_font_scale = parse_font_scale(content);
    let target_glyph_budget = target_glyph_budget(target_font_scale);
    let pane_state = state.entry(pane_id).or_default();

    let atlas_cache_policy =
        pane_state
            .last_font_scale
            .map_or(FontAtlasCachePolicy::FullRebuild, |last| {
                let delta = (target_font_scale - last).abs();
                if delta <= 0.03 {
                    FontAtlasCachePolicy::ReuseHotAtlas
                } else if delta <= 0.20 {
                    FontAtlasCachePolicy::SelectiveInvalidate
                } else {
                    FontAtlasCachePolicy::FullRebuild
                }
            });

    let cache_hit_glyphs = match atlas_cache_policy {
        FontAtlasCachePolicy::ReuseHotAtlas => pane_state.cached_glyphs.min(target_glyph_budget),
        FontAtlasCachePolicy::SelectiveInvalidate => {
            (pane_state.cached_glyphs / 2).min(target_glyph_budget)
        }
        FontAtlasCachePolicy::FullRebuild => 0,
    };

    let glyphs_to_rebuild = target_glyph_budget.saturating_sub(cache_hit_glyphs);
    let staged_batch_size = 256u32;
    let staged_batches_total = if glyphs_to_rebuild == 0 {
        0
    } else {
        div_ceil_u32(glyphs_to_rebuild, staged_batch_size)
    };
    let glyphs_rebuilt_now = glyphs_to_rebuild.min(staged_batch_size);
    let deferred_glyphs = glyphs_to_rebuild.saturating_sub(glyphs_rebuilt_now);
    let staged_batches_deferred = staged_batches_total.saturating_sub(1);

    let shader_warmup = !pane_state.shader_warmed
        || matches!(atlas_cache_policy, FontAtlasCachePolicy::FullRebuild);

    // Stage expensive font-switch work into bounded chunks to avoid single-frame stalls.
    let cache_probe_ns = 20_000u64;
    let atlas_rebuild_ns = u64::from(glyphs_rebuilt_now).saturating_mul(700);
    let shader_warmup_ns = if shader_warmup { 120_000 } else { 0 };
    let cache_commit_ns = 30_000u64;
    let duration_ns = cache_probe_ns
        .saturating_add(atlas_rebuild_ns)
        .saturating_add(shader_warmup_ns)
        .saturating_add(cache_commit_ns)
        .max(1);

    pane_state.last_font_scale = Some(target_font_scale);
    pane_state.cached_glyphs = target_glyph_budget;
    pane_state.shader_warmed = true;

    (
        duration_ns,
        Some(FontRenderPrepMetrics {
            atlas_cache_policy,
            shader_warmup,
            cache_hit_glyphs,
            glyphs_rebuilt_now,
            deferred_glyphs,
            staged_batches_total,
            staged_batches_deferred,
        }),
    )
}

fn parse_font_scale(content: &str) -> f64 {
    content
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(1.0)
}

fn target_glyph_budget(font_scale: f64) -> u32 {
    let scaled = (font_scale * 1024.0).round();
    if !scaled.is_finite() || scaled <= 0.0 {
        return 1024;
    }
    let clamped = scaled.clamp(256.0, 4096.0);
    clamped as u32
}

fn div_ceil_u32(numerator: u32, denominator: u32) -> u32 {
    if denominator == 0 {
        numerator
    } else {
        numerator.saturating_add(denominator.saturating_sub(1)) / denominator
    }
}

fn duration_ns_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn ns_to_ms_u64(duration_ns: u64) -> u64 {
    duration_ns / 1_000_000
}

fn epoch_ms_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wezterm::WeztermInterface;
    use std::collections::BTreeSet;

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
    action: set_font_size
    content: "1.15"
  - at: "6s"
    pane: 0
    action: generate_scrollback
    content: "8x64"
  - at: "7s"
    pane: 0
    action: typing
    content: "hello"
  - at: "8s"
    pane: 0
    action: paste
    content: "multi-line payload"
  - at: "9s"
    pane: 0
    action: mouse
    name: left_click
  - at: "10s"
    pane: 0
    action: marker
    name: checkpoint
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        assert_eq!(scenario.events.len(), 10);
        assert_eq!(scenario.events[0].action, EventAction::Append);
        assert_eq!(scenario.events[1].action, EventAction::Clear);
        assert_eq!(scenario.events[2].action, EventAction::SetTitle);
        assert_eq!(scenario.events[3].action, EventAction::Resize);
        assert_eq!(scenario.events[4].action, EventAction::SetFontSize);
        assert_eq!(scenario.events[5].action, EventAction::GenerateScrollback);
        assert_eq!(scenario.events[6].action, EventAction::Typing);
        assert_eq!(scenario.events[7].action, EventAction::Paste);
        assert_eq!(scenario.events[8].action, EventAction::Mouse);
        assert_eq!(scenario.events[9].action, EventAction::Marker);
    }

    #[test]
    fn event_action_resize_timeline_helpers() {
        assert_eq!(EventAction::Resize.as_str(), "resize");
        assert_eq!(EventAction::SetFontSize.as_str(), "set_font_size");
        assert_eq!(
            EventAction::GenerateScrollback.as_str(),
            "generate_scrollback"
        );
        assert_eq!(EventAction::Typing.as_str(), "typing");
        assert_eq!(EventAction::Paste.as_str(), "paste");
        assert_eq!(EventAction::Mouse.as_str(), "mouse");
        assert!(!EventAction::Append.is_resize_timeline_action());
        assert!(EventAction::Resize.is_resize_timeline_action());
        assert!(EventAction::SetFontSize.is_resize_timeline_action());
        assert!(EventAction::GenerateScrollback.is_resize_timeline_action());
        assert!(EventAction::Typing.is_resize_timeline_action());
        assert!(EventAction::Paste.is_resize_timeline_action());
        assert!(EventAction::Mouse.is_resize_timeline_action());
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

    #[test]
    fn to_mock_event_set_font_size_marker() {
        let event = ScenarioEvent {
            at: Duration::from_secs(1),
            pane: 0,
            action: EventAction::SetFontSize,
            content: "1.25".to_string(),
            name: String::new(),
            comment: None,
        };
        let mock_event = Scenario::to_mock_event(&event).unwrap();
        assert!(matches!(mock_event, MockEvent::AppendOutput(ref s) if s == "[FONT_SIZE:1.25]"));
    }

    #[test]
    fn to_mock_event_generate_scrollback() {
        let event = ScenarioEvent {
            at: Duration::from_secs(1),
            pane: 0,
            action: EventAction::GenerateScrollback,
            content: "3x40".to_string(),
            name: String::new(),
            comment: None,
        };
        let mock_event = Scenario::to_mock_event(&event).unwrap();
        match mock_event {
            MockEvent::AppendOutput(text) => {
                let lines: Vec<&str> = text.lines().collect();
                assert_eq!(lines.len(), 3);
                assert!(lines.iter().all(|line| line.len() == 40));
                assert!(lines[0].contains("[scrollback:000000]"));
            }
            _ => panic!("expected generated scrollback append output"),
        }
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
    async fn execute_all_with_resize_timeline_records_stage_probes() {
        let yaml = r#"
name: resize_probe_case
duration: "10s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: append
    content: "bootstrap\n"
  - at: "2s"
    pane: 0
    action: resize
    content: "120x40"
  - at: "3s"
    pane: 0
    action: set_font_size
    content: "1.15"
  - at: "4s"
    pane: 0
    action: generate_scrollback
    content: "4x48"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let (executed, timeline) = scenario
            .execute_all_with_resize_timeline(&mock)
            .await
            .unwrap();
        assert_eq!(executed, scenario.events.len());
        assert_eq!(timeline.executed_resize_events, 3);
        assert_eq!(timeline.events.len(), 3);

        for event in &timeline.events {
            assert_eq!(event.sequence_no, event.event_index as u64);
            assert_eq!(event.frame_id, event.sequence_no);
            assert_eq!(event.scheduler_decision, "dequeue_latest_intent");
            assert_eq!(event.test_case_id, scenario.name);
            assert!(event.resize_transaction_id.starts_with(&format!(
                "{}:{}",
                timeline.reproducibility_key, event.event_index
            )));
            assert_eq!(event.stages.len(), ResizeTimelineStage::ALL.len());
            for (sample, expected) in event.stages.iter().zip(ResizeTimelineStage::ALL.iter()) {
                assert_eq!(sample.stage, *expected);
            }
            assert_eq!(
                event.queue_wait_ms,
                ns_to_ms_u64(event.stages[1].duration_ns)
            );
            assert_eq!(event.reflow_ms, ns_to_ms_u64(event.stages[2].duration_ns));
            assert_eq!(event.render_ms, ns_to_ms_u64(event.stages[3].duration_ns));
            assert_eq!(event.present_ms, ns_to_ms_u64(event.stages[4].duration_ns));
            let render_prep_metrics = event.stages[3].render_prep_metrics.as_ref();
            if event.action == EventAction::SetFontSize {
                let metrics = render_prep_metrics
                    .expect("set_font_size events should emit render-prep metrics");
                assert!(metrics.staged_batches_total >= 1);
                assert!(metrics.glyphs_rebuilt_now > 0 || metrics.cache_hit_glyphs > 0);
            } else {
                assert!(
                    render_prep_metrics.is_none(),
                    "non-font resize events should not emit font render-prep metrics"
                );
            }
            let queue = event.stages[1].queue_metrics.as_ref().unwrap();
            assert!(
                queue.depth_before >= queue.depth_after,
                "queue depth should be non-increasing for dequeued event"
            );
        }
    }

    #[tokio::test]
    async fn resize_timeline_summary_and_flame_samples_cover_all_stages() {
        let yaml = r#"
name: resize_probe_summary
duration: "6s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: resize
    content: "100x30"
  - at: "2s"
    pane: 0
    action: set_font_size
    content: "1.20"
  - at: "3s"
    pane: 0
    action: generate_scrollback
    content: "5x60"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let (_executed, timeline) = scenario
            .execute_all_with_resize_timeline(&mock)
            .await
            .unwrap();
        let summary = timeline.stage_summary();
        assert_eq!(summary.len(), ResizeTimelineStage::ALL.len());
        assert!(summary.iter().all(|entry| entry.samples == 3));
        assert!(summary.iter().all(|entry| {
            entry.p50_duration_ns <= entry.p95_duration_ns
                && entry.p95_duration_ns <= entry.p99_duration_ns
                && entry.p99_duration_ns <= entry.max_duration_ns
                && entry.total_duration_ns >= entry.max_duration_ns
        }));

        let flame = timeline.flame_samples();
        assert_eq!(
            flame.len(),
            timeline.events.len() * ResizeTimelineStage::ALL.len()
        );
        let mut stage_suffixes = BTreeSet::new();
        for row in &flame {
            let suffix = row.stack.rsplit(';').next().unwrap_or_default().to_string();
            stage_suffixes.insert(suffix);
        }
        for stage in ResizeTimelineStage::ALL {
            assert!(stage_suffixes.contains(stage.as_str()));
        }
    }

    #[tokio::test]
    async fn set_font_size_render_prep_uses_staged_atlas_and_shader_warmup_policy() {
        let yaml = r#"
name: font_pipeline_policy
duration: "8s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: set_font_size
    content: "1.00"
  - at: "2s"
    pane: 0
    action: set_font_size
    content: "1.02"
  - at: "3s"
    pane: 0
    action: set_font_size
    content: "1.60"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let (_executed, timeline) = scenario
            .execute_all_with_resize_timeline(&mock)
            .await
            .unwrap();
        assert_eq!(timeline.events.len(), 3);

        let first = timeline.events[0].stages[3]
            .render_prep_metrics
            .as_ref()
            .unwrap();
        assert_eq!(first.atlas_cache_policy, FontAtlasCachePolicy::FullRebuild);
        assert!(first.shader_warmup);
        assert!(first.staged_batches_total >= 1);

        let second = timeline.events[1].stages[3]
            .render_prep_metrics
            .as_ref()
            .unwrap();
        assert_eq!(
            second.atlas_cache_policy,
            FontAtlasCachePolicy::ReuseHotAtlas
        );
        assert!(!second.shader_warmup);
        assert!(second.cache_hit_glyphs > 0);

        let third = timeline.events[2].stages[3]
            .render_prep_metrics
            .as_ref()
            .unwrap();
        assert_eq!(third.atlas_cache_policy, FontAtlasCachePolicy::FullRebuild);
        assert!(third.shader_warmup);
        assert!(third.deferred_glyphs > 0);
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
        assert_eq!(pane.window_id, 0);
        assert_eq!(pane.tab_id, 0);
        assert_eq!(pane.cols, 80);
        assert_eq!(pane.rows, 24);
        assert!(pane.initial_content.is_empty());
        assert!(scenario.metadata.is_empty());
    }

    #[test]
    fn parse_metadata_and_reproducibility_key() {
        let yaml = r#"
name: metadata_case
duration: "5s"
metadata:
  suite: resize_baseline
  suite_version: "2026-02-13"
  seed: "424242"
  scale_profile: multi_tab_storm
panes:
  - id: 0
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        assert_eq!(
            scenario.reproducibility_key(),
            "resize_baseline:2026-02-13:metadata_case:424242"
        );
        assert_eq!(
            scenario.metadata.get("scale_profile"),
            Some(&"multi_tab_storm".to_string())
        );
    }

    #[test]
    fn metadata_empty_values_are_rejected() {
        let yaml = r#"
name: bad_meta
duration: "5s"
metadata:
  suite: ""
panes:
  - id: 0
events: []
"#;
        assert!(Scenario::from_yaml(yaml).is_err());
    }

    #[test]
    fn generate_scrollback_validation_rejects_bad_specs() {
        let yaml = r#"
name: bad_scrollback
duration: "5s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: generate_scrollback
    content: "0x80"
"#;
        assert!(Scenario::from_yaml(yaml).is_err());
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

        sandbox.log_command("ft status", Some("basics.1"));
        sandbox.log_command("ft list", None);

        assert_eq!(sandbox.command_log().len(), 2);
        assert_eq!(sandbox.command_log()[0].command, "ft status");
        assert_eq!(
            sandbox.command_log()[0].exercise_id.as_deref(),
            Some("basics.1")
        );
        assert_eq!(sandbox.command_log()[1].command, "ft list");
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

    // -----------------------------------------------------------------------
    // NEW TESTS: parse_duration edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn parse_duration_bare_number_treated_as_seconds() {
        // A bare number with no unit is treated as seconds
        assert_eq!(parse_duration("10").unwrap(), Duration::from_secs(10));
    }

    #[test]
    fn parse_duration_leading_trailing_whitespace() {
        assert_eq!(parse_duration("  5s  ").unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn parse_duration_zero() {
        assert_eq!(parse_duration("0s").unwrap(), Duration::from_millis(0));
    }

    #[test]
    fn parse_duration_fractional_minutes() {
        // 0.5m = 30s
        assert_eq!(parse_duration("0.5m").unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn parse_duration_fractional_hours() {
        // 0.5h = 30m = 1800s
        assert_eq!(parse_duration("0.5h").unwrap(), Duration::from_secs(1800));
    }

    #[test]
    fn parse_duration_large_value() {
        // 24h = 86400s
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86400));
    }

    #[test]
    fn parse_duration_hour_minute_second() {
        // 1h2m3s = 3723s
        assert_eq!(parse_duration("1h2m3s").unwrap(), Duration::from_secs(3723));
    }

    #[test]
    fn parse_duration_unknown_unit_returns_error() {
        let err = parse_duration("5d").unwrap_err();
        assert!(err.contains("Unknown duration unit"));
    }

    #[test]
    fn parse_duration_invalid_number() {
        let err = parse_duration("abcs").unwrap_err();
        assert!(err.contains("Invalid number"));
    }

    #[test]
    fn parse_duration_empty_string() {
        // Empty after trim => empty num_buf, 0ms
        assert_eq!(parse_duration("").unwrap(), Duration::from_millis(0));
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: parse_resize_spec edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn parse_resize_spec_valid() {
        assert_eq!(parse_resize_spec("80x24").unwrap(), (80, 24));
    }

    #[test]
    fn parse_resize_spec_uppercase_x() {
        assert_eq!(parse_resize_spec("120X40").unwrap(), (120, 40));
    }

    #[test]
    fn parse_resize_spec_with_whitespace() {
        assert_eq!(parse_resize_spec(" 80 x 24 ").unwrap(), (80, 24));
    }

    #[test]
    fn parse_resize_spec_zero_cols() {
        let err = parse_resize_spec("0x24");
        assert!(err.is_err());
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("must be > 0"));
    }

    #[test]
    fn parse_resize_spec_zero_rows() {
        let err = parse_resize_spec("80x0");
        assert!(err.is_err());
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("must be > 0"));
    }

    #[test]
    fn parse_resize_spec_no_separator() {
        assert!(parse_resize_spec("8024").is_err());
    }

    #[test]
    fn parse_resize_spec_non_numeric_cols() {
        assert!(parse_resize_spec("abcx24").is_err());
    }

    #[test]
    fn parse_resize_spec_non_numeric_rows() {
        assert!(parse_resize_spec("80xabc").is_err());
    }

    #[test]
    fn parse_resize_spec_large_dimensions() {
        assert_eq!(parse_resize_spec("4096x2160").unwrap(), (4096, 2160));
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: parse_scrollback_spec edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn parse_scrollback_spec_lines_only_defaults_width_96() {
        let (lines, width) = parse_scrollback_spec("100").unwrap();
        assert_eq!(lines, 100);
        assert_eq!(width, 96);
    }

    #[test]
    fn parse_scrollback_spec_lines_and_width() {
        let (lines, width) = parse_scrollback_spec("50x80").unwrap();
        assert_eq!(lines, 50);
        assert_eq!(width, 80);
    }

    #[test]
    fn parse_scrollback_spec_uppercase_x() {
        let (lines, width) = parse_scrollback_spec("10X40").unwrap();
        assert_eq!(lines, 10);
        assert_eq!(width, 40);
    }

    #[test]
    fn parse_scrollback_spec_empty_is_error() {
        assert!(parse_scrollback_spec("").is_err());
        assert!(parse_scrollback_spec("   ").is_err());
    }

    #[test]
    fn parse_scrollback_spec_zero_lines() {
        let err = parse_scrollback_spec("0x80");
        assert!(err.is_err());
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("must be > 0"));
    }

    #[test]
    fn parse_scrollback_spec_too_many_lines() {
        let err = parse_scrollback_spec("300000x80");
        assert!(err.is_err());
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("too large"));
    }

    #[test]
    fn parse_scrollback_spec_max_lines_boundary() {
        // Exactly 250000 should succeed
        let (lines, _width) = parse_scrollback_spec("250000").unwrap();
        assert_eq!(lines, 250_000);
    }

    #[test]
    fn parse_scrollback_spec_over_max_lines_boundary() {
        // 250001 should fail
        assert!(parse_scrollback_spec("250001").is_err());
    }

    #[test]
    fn parse_scrollback_spec_width_too_small() {
        let err = parse_scrollback_spec("10x19");
        assert!(err.is_err());
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("out of range"));
    }

    #[test]
    fn parse_scrollback_spec_width_too_large() {
        let err = parse_scrollback_spec("10x4097");
        assert!(err.is_err());
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("out of range"));
    }

    #[test]
    fn parse_scrollback_spec_width_boundary_min() {
        let (lines, width) = parse_scrollback_spec("1x20").unwrap();
        assert_eq!(lines, 1);
        assert_eq!(width, 20);
    }

    #[test]
    fn parse_scrollback_spec_width_boundary_max() {
        let (lines, width) = parse_scrollback_spec("1x4096").unwrap();
        assert_eq!(lines, 1);
        assert_eq!(width, 4096);
    }

    #[test]
    fn parse_scrollback_spec_invalid_lines() {
        assert!(parse_scrollback_spec("notanumber").is_err());
    }

    #[test]
    fn parse_scrollback_spec_invalid_width() {
        assert!(parse_scrollback_spec("10xnotanumber").is_err());
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: generate_scrollback content verification
    // -----------------------------------------------------------------------

    #[test]
    fn generate_scrollback_content_has_correct_line_count() {
        let text = generate_scrollback(5, 40);
        assert_eq!(text.lines().count(), 5);
    }

    #[test]
    fn generate_scrollback_each_line_has_correct_width() {
        let text = generate_scrollback(3, 60);
        for line in text.lines() {
            assert_eq!(line.len(), 60);
        }
    }

    #[test]
    fn generate_scrollback_lines_have_monotonic_indices() {
        let text = generate_scrollback(10, 96);
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let expected_prefix = format!("[scrollback:{:06}]", i);
            assert!(
                line.starts_with(&expected_prefix),
                "Line {} should start with '{}', got '{}'",
                i,
                expected_prefix,
                &line[..expected_prefix.len().min(line.len())]
            );
        }
    }

    #[test]
    fn generate_scrollback_single_line() {
        let text = generate_scrollback(1, 30);
        assert_eq!(text.lines().count(), 1);
        assert_eq!(text.lines().next().unwrap().len(), 30);
    }

    #[test]
    fn generate_scrollback_minimum_width() {
        let text = generate_scrollback(2, 20);
        for line in text.lines() {
            assert_eq!(line.len(), 20);
        }
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: EventAction exhaustive coverage
    // -----------------------------------------------------------------------

    #[test]
    fn event_action_as_str_all_variants() {
        assert_eq!(EventAction::Append.as_str(), "append");
        assert_eq!(EventAction::Clear.as_str(), "clear");
        assert_eq!(EventAction::SetTitle.as_str(), "set_title");
        assert_eq!(EventAction::Resize.as_str(), "resize");
        assert_eq!(EventAction::SetFontSize.as_str(), "set_font_size");
        assert_eq!(
            EventAction::GenerateScrollback.as_str(),
            "generate_scrollback"
        );
        assert_eq!(EventAction::Typing.as_str(), "typing");
        assert_eq!(EventAction::Paste.as_str(), "paste");
        assert_eq!(EventAction::Mouse.as_str(), "mouse");
        assert_eq!(EventAction::Marker.as_str(), "marker");
    }

    #[test]
    fn event_action_is_resize_timeline_exhaustive() {
        assert!(!EventAction::Append.is_resize_timeline_action());
        assert!(!EventAction::Clear.is_resize_timeline_action());
        assert!(!EventAction::SetTitle.is_resize_timeline_action());
        assert!(EventAction::Resize.is_resize_timeline_action());
        assert!(EventAction::SetFontSize.is_resize_timeline_action());
        assert!(EventAction::GenerateScrollback.is_resize_timeline_action());
        assert!(EventAction::Typing.is_resize_timeline_action());
        assert!(EventAction::Paste.is_resize_timeline_action());
        assert!(EventAction::Mouse.is_resize_timeline_action());
        assert!(!EventAction::Marker.is_resize_timeline_action());
    }

    #[test]
    fn event_action_clone_and_eq() {
        let action = EventAction::Resize;
        let cloned = action.clone();
        assert_eq!(action, cloned);
        assert_ne!(EventAction::Append, EventAction::Clear);
    }

    #[test]
    fn event_action_debug_format() {
        let dbg = format!("{:?}", EventAction::GenerateScrollback);
        assert_eq!(dbg, "GenerateScrollback");
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: ResizeTimelineStage
    // -----------------------------------------------------------------------

    #[test]
    fn resize_timeline_stage_all_has_five_entries() {
        assert_eq!(ResizeTimelineStage::ALL.len(), 5);
    }

    #[test]
    fn resize_timeline_stage_as_str_all_variants() {
        assert_eq!(ResizeTimelineStage::InputIntent.as_str(), "input_intent");
        assert_eq!(
            ResizeTimelineStage::SchedulerQueueing.as_str(),
            "scheduler_queueing"
        );
        assert_eq!(
            ResizeTimelineStage::LogicalReflow.as_str(),
            "logical_reflow"
        );
        assert_eq!(ResizeTimelineStage::RenderPrep.as_str(), "render_prep");
        assert_eq!(ResizeTimelineStage::Presentation.as_str(), "presentation");
    }

    #[test]
    fn resize_timeline_stage_ordering() {
        assert!(ResizeTimelineStage::InputIntent < ResizeTimelineStage::SchedulerQueueing);
        assert!(ResizeTimelineStage::SchedulerQueueing < ResizeTimelineStage::LogicalReflow);
        assert!(ResizeTimelineStage::LogicalReflow < ResizeTimelineStage::RenderPrep);
        assert!(ResizeTimelineStage::RenderPrep < ResizeTimelineStage::Presentation);
    }

    #[test]
    fn resize_timeline_stage_all_order_matches_ord() {
        for pair in ResizeTimelineStage::ALL.windows(2) {
            assert!(pair[0] < pair[1]);
        }
    }

    #[test]
    fn resize_timeline_stage_hash_distinct() {
        use std::collections::HashSet;
        let set: HashSet<ResizeTimelineStage> = ResizeTimelineStage::ALL.iter().copied().collect();
        assert_eq!(set.len(), 5);
    }

    #[test]
    fn resize_timeline_stage_copy_semantics() {
        let stage = ResizeTimelineStage::LogicalReflow;
        let copied = stage;
        assert_eq!(stage, copied);
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: Serde roundtrips (JSON)
    // -----------------------------------------------------------------------

    #[test]
    fn event_action_serde_json_roundtrip() {
        for action in [
            EventAction::Append,
            EventAction::Clear,
            EventAction::SetTitle,
            EventAction::Resize,
            EventAction::SetFontSize,
            EventAction::GenerateScrollback,
            EventAction::Typing,
            EventAction::Paste,
            EventAction::Mouse,
            EventAction::Marker,
        ] {
            let json = serde_json::to_string(&action).unwrap();
            let back: EventAction = serde_json::from_str(&json).unwrap();
            assert_eq!(action, back);
        }
    }

    #[test]
    fn event_action_serde_snake_case() {
        let json = serde_json::to_string(&EventAction::SetFontSize).unwrap();
        assert_eq!(json, "\"set_font_size\"");
        let json = serde_json::to_string(&EventAction::GenerateScrollback).unwrap();
        assert_eq!(json, "\"generate_scrollback\"");
        let json = serde_json::to_string(&EventAction::Typing).unwrap();
        assert_eq!(json, "\"typing\"");
        let json = serde_json::to_string(&EventAction::Paste).unwrap();
        assert_eq!(json, "\"paste\"");
        let json = serde_json::to_string(&EventAction::Mouse).unwrap();
        assert_eq!(json, "\"mouse\"");
    }

    #[test]
    fn resize_timeline_stage_serde_json_roundtrip() {
        for stage in ResizeTimelineStage::ALL {
            let json = serde_json::to_string(&stage).unwrap();
            let back: ResizeTimelineStage = serde_json::from_str(&json).unwrap();
            assert_eq!(stage, back);
        }
    }

    #[test]
    fn resize_timeline_stage_serde_snake_case() {
        let json = serde_json::to_string(&ResizeTimelineStage::SchedulerQueueing).unwrap();
        assert_eq!(json, "\"scheduler_queueing\"");
    }

    #[test]
    fn resize_queue_metrics_serde_json_roundtrip() {
        let m = ResizeQueueMetrics {
            depth_before: 10,
            depth_after: 9,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ResizeQueueMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn font_atlas_cache_policy_serde_roundtrip() {
        for policy in [
            FontAtlasCachePolicy::ReuseHotAtlas,
            FontAtlasCachePolicy::SelectiveInvalidate,
            FontAtlasCachePolicy::FullRebuild,
        ] {
            let json = serde_json::to_string(&policy).unwrap();
            let back: FontAtlasCachePolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(policy, back);
        }
    }

    #[test]
    fn font_render_prep_metrics_serde_roundtrip() {
        let metrics = FontRenderPrepMetrics {
            atlas_cache_policy: FontAtlasCachePolicy::SelectiveInvalidate,
            shader_warmup: true,
            cache_hit_glyphs: 120,
            glyphs_rebuilt_now: 64,
            deferred_glyphs: 32,
            staged_batches_total: 2,
            staged_batches_deferred: 1,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        let back: FontRenderPrepMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(metrics, back);
    }

    #[test]
    fn resize_timeline_stage_sample_serde_with_queue_metrics() {
        let sample = ResizeTimelineStageSample {
            stage: ResizeTimelineStage::SchedulerQueueing,
            start_offset_ns: 100,
            duration_ns: 500,
            queue_metrics: Some(ResizeQueueMetrics {
                depth_before: 3,
                depth_after: 2,
            }),
            render_prep_metrics: None,
        };
        let json = serde_json::to_string(&sample).unwrap();
        assert!(json.contains("queue_metrics"));
        let back: ResizeTimelineStageSample = serde_json::from_str(&json).unwrap();
        assert_eq!(sample, back);
    }

    #[test]
    fn resize_timeline_stage_sample_serde_without_queue_metrics() {
        let sample = ResizeTimelineStageSample {
            stage: ResizeTimelineStage::InputIntent,
            start_offset_ns: 0,
            duration_ns: 42,
            queue_metrics: None,
            render_prep_metrics: None,
        };
        let json = serde_json::to_string(&sample).unwrap();
        // skip_serializing_if means queue_metrics should not appear
        assert!(!json.contains("queue_metrics"));
        assert!(!json.contains("render_prep_metrics"));
        let back: ResizeTimelineStageSample = serde_json::from_str(&json).unwrap();
        assert_eq!(sample, back);
    }

    #[test]
    fn resize_timeline_stage_sample_serde_with_render_prep_metrics() {
        let sample = ResizeTimelineStageSample {
            stage: ResizeTimelineStage::RenderPrep,
            start_offset_ns: 250,
            duration_ns: 900,
            queue_metrics: None,
            render_prep_metrics: Some(FontRenderPrepMetrics {
                atlas_cache_policy: FontAtlasCachePolicy::ReuseHotAtlas,
                shader_warmup: false,
                cache_hit_glyphs: 400,
                glyphs_rebuilt_now: 120,
                deferred_glyphs: 30,
                staged_batches_total: 2,
                staged_batches_deferred: 1,
            }),
        };
        let json = serde_json::to_string(&sample).unwrap();
        assert!(json.contains("render_prep_metrics"));
        let back: ResizeTimelineStageSample = serde_json::from_str(&json).unwrap();
        assert_eq!(sample, back);
    }

    #[test]
    fn resize_timeline_flame_sample_serde_roundtrip() {
        let fs = ResizeTimelineFlameSample {
            stack: "test;resize;input_intent".to_string(),
            duration_ns: 12345,
            event_index: 0,
            pane_id: 42,
        };
        let json = serde_json::to_string(&fs).unwrap();
        let back: ResizeTimelineFlameSample = serde_json::from_str(&json).unwrap();
        assert_eq!(fs, back);
    }

    #[test]
    fn scenario_pane_serde_json_roundtrip() {
        let pane = ScenarioPane {
            id: 5,
            title: "Test".to_string(),
            domain: "remote".to_string(),
            cwd: "/tmp".to_string(),
            window_id: 1,
            tab_id: 2,
            cols: 120,
            rows: 40,
            initial_content: "hello".to_string(),
        };
        let json = serde_json::to_string(&pane).unwrap();
        let back: ScenarioPane = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 5);
        assert_eq!(back.title, "Test");
        assert_eq!(back.cols, 120);
        assert_eq!(back.rows, 40);
        assert_eq!(back.initial_content, "hello");
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: Scenario validation edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn validate_empty_set_font_size_content() {
        let yaml = r#"
name: bad_font_size
duration: "5s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: set_font_size
    content: ""
"#;
        let err = Scenario::from_yaml(yaml);
        assert!(err.is_err());
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("SetFontSize requires non-empty content"));
    }

    #[test]
    fn validate_set_font_size_whitespace_only() {
        let yaml = r#"
name: bad_font_size_ws
duration: "5s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: set_font_size
    content: "   "
"#;
        let err = Scenario::from_yaml(yaml);
        assert!(err.is_err());
    }

    #[test]
    fn validate_resize_bad_spec_in_scenario() {
        let yaml = r#"
name: bad_resize_spec
duration: "5s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: resize
    content: "not_a_resize"
"#;
        assert!(Scenario::from_yaml(yaml).is_err());
    }

    #[test]
    fn validate_resize_zero_dimensions_in_scenario() {
        let yaml = r#"
name: zero_resize
duration: "5s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: resize
    content: "0x0"
"#;
        assert!(Scenario::from_yaml(yaml).is_err());
    }

    #[test]
    fn validate_events_at_same_time_is_ok() {
        let yaml = r#"
name: same_time
duration: "5s"
panes:
  - id: 0
events:
  - at: "2s"
    pane: 0
    action: append
    content: "a"
  - at: "2s"
    pane: 0
    action: append
    content: "b"
"#;
        // Same time is not out-of-order, it should be valid
        assert!(Scenario::from_yaml(yaml).is_ok());
    }

    #[test]
    fn validate_single_event_always_in_order() {
        let yaml = r#"
name: single_event
duration: "5s"
panes:
  - id: 0
events:
  - at: "3s"
    pane: 0
    action: append
    content: "only one"
"#;
        assert!(Scenario::from_yaml(yaml).is_ok());
    }

    #[test]
    fn validate_many_panes_unique_ids() {
        let yaml = r#"
name: many_panes
duration: "5s"
panes:
  - id: 0
  - id: 1
  - id: 2
  - id: 100
  - id: 999
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        assert_eq!(scenario.panes.len(), 5);
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: Reproducibility key variants
    // -----------------------------------------------------------------------

    #[test]
    fn reproducibility_key_defaults_when_no_metadata() {
        let yaml = r#"
name: bare
duration: "1s"
panes: []
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        assert_eq!(scenario.reproducibility_key(), "ad_hoc:v1:bare:0");
    }

    #[test]
    fn reproducibility_key_partial_metadata() {
        let yaml = r#"
name: partial
duration: "1s"
metadata:
  suite: my_suite
panes: []
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        // suite_version and seed should use defaults
        assert_eq!(scenario.reproducibility_key(), "my_suite:v1:partial:0");
    }

    #[test]
    fn reproducibility_key_only_seed() {
        let yaml = r#"
name: seeded
duration: "1s"
metadata:
  seed: "42"
panes: []
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        assert_eq!(scenario.reproducibility_key(), "ad_hoc:v1:seeded:42");
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: ResizeTimeline summary/flame edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn empty_timeline_stage_summary_returns_zero_samples() {
        let timeline = ResizeTimeline {
            scenario: "empty".to_string(),
            reproducibility_key: "test:v1:empty:0".to_string(),
            captured_at_ms: 0,
            executed_resize_events: 0,
            events: vec![],
        };
        let summary = timeline.stage_summary();
        assert_eq!(summary.len(), 5); // all 5 stages present
        for entry in &summary {
            assert_eq!(entry.samples, 0);
            assert_eq!(entry.total_duration_ns, 0);
            assert!(entry.avg_duration_ns.abs() < f64::EPSILON);
            assert_eq!(entry.p50_duration_ns, 0);
            assert_eq!(entry.p95_duration_ns, 0);
            assert_eq!(entry.p99_duration_ns, 0);
            assert_eq!(entry.max_duration_ns, 0);
        }
    }

    #[test]
    fn empty_timeline_flame_samples_returns_empty() {
        let timeline = ResizeTimeline {
            scenario: "empty".to_string(),
            reproducibility_key: "test:v1:empty:0".to_string(),
            captured_at_ms: 0,
            executed_resize_events: 0,
            events: vec![],
        };
        assert!(timeline.flame_samples().is_empty());
    }

    #[test]
    fn timeline_summary_single_sample_percentiles() {
        // With a single sample, p50/p95/p99/max should all equal that sample
        let timeline = ResizeTimeline {
            scenario: "one".to_string(),
            reproducibility_key: "test:v1:one:0".to_string(),
            captured_at_ms: 100,
            executed_resize_events: 1,
            events: vec![ResizeTimelineEvent {
                event_index: 0,
                resize_transaction_id: "t:0".to_string(),
                pane_id: 0,
                tab_id: 0,
                sequence_no: 0,
                action: EventAction::Resize,
                scheduler_decision: "dequeue_latest_intent".to_string(),
                frame_id: 0,
                test_case_id: "one".to_string(),
                queue_wait_ms: 0,
                reflow_ms: 0,
                render_ms: 0,
                present_ms: 0,
                scheduled_at_ns: 0,
                dispatch_offset_ns: 0,
                total_duration_ns: 1000,
                stages: vec![
                    ResizeTimelineStageSample {
                        stage: ResizeTimelineStage::InputIntent,
                        start_offset_ns: 0,
                        duration_ns: 100,
                        queue_metrics: None,
                        render_prep_metrics: None,
                    },
                    ResizeTimelineStageSample {
                        stage: ResizeTimelineStage::SchedulerQueueing,
                        start_offset_ns: 100,
                        duration_ns: 200,
                        queue_metrics: Some(ResizeQueueMetrics {
                            depth_before: 1,
                            depth_after: 0,
                        }),
                        render_prep_metrics: None,
                    },
                    ResizeTimelineStageSample {
                        stage: ResizeTimelineStage::LogicalReflow,
                        start_offset_ns: 300,
                        duration_ns: 300,
                        queue_metrics: None,
                        render_prep_metrics: None,
                    },
                    ResizeTimelineStageSample {
                        stage: ResizeTimelineStage::RenderPrep,
                        start_offset_ns: 600,
                        duration_ns: 150,
                        queue_metrics: None,
                        render_prep_metrics: None,
                    },
                    ResizeTimelineStageSample {
                        stage: ResizeTimelineStage::Presentation,
                        start_offset_ns: 750,
                        duration_ns: 250,
                        queue_metrics: None,
                        render_prep_metrics: None,
                    },
                ],
            }],
        };
        let summary = timeline.stage_summary();
        for entry in &summary {
            assert_eq!(entry.samples, 1);
            assert_eq!(entry.p50_duration_ns, entry.max_duration_ns);
            assert_eq!(entry.p95_duration_ns, entry.max_duration_ns);
            assert_eq!(entry.p99_duration_ns, entry.max_duration_ns);
            assert!((entry.avg_duration_ns - entry.total_duration_ns as f64).abs() < f64::EPSILON);
        }
        // Verify a specific stage
        let input_intent_summary = summary
            .iter()
            .find(|s| s.stage == ResizeTimelineStage::InputIntent)
            .unwrap();
        assert_eq!(input_intent_summary.total_duration_ns, 100);
    }

    #[test]
    fn flame_sample_stack_format() {
        let timeline = ResizeTimeline {
            scenario: "my_scenario".to_string(),
            reproducibility_key: "key".to_string(),
            captured_at_ms: 0,
            executed_resize_events: 1,
            events: vec![ResizeTimelineEvent {
                event_index: 0,
                resize_transaction_id: "t:0".to_string(),
                pane_id: 7,
                tab_id: 0,
                sequence_no: 0,
                action: EventAction::SetFontSize,
                scheduler_decision: "dequeue_latest_intent".to_string(),
                frame_id: 0,
                test_case_id: "test".to_string(),
                queue_wait_ms: 0,
                reflow_ms: 0,
                render_ms: 0,
                present_ms: 0,
                scheduled_at_ns: 0,
                dispatch_offset_ns: 0,
                total_duration_ns: 100,
                stages: vec![ResizeTimelineStageSample {
                    stage: ResizeTimelineStage::Presentation,
                    start_offset_ns: 0,
                    duration_ns: 100,
                    queue_metrics: None,
                    render_prep_metrics: None,
                }],
            }],
        };
        let flames = timeline.flame_samples();
        assert_eq!(flames.len(), 1);
        assert_eq!(flames[0].stack, "my_scenario;set_font_size;presentation");
        assert_eq!(flames[0].pane_id, 7);
        assert_eq!(flames[0].event_index, 0);
        assert_eq!(flames[0].duration_ns, 100);
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: ResizeTimeline serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn resize_timeline_serde_json_roundtrip() {
        let timeline = ResizeTimeline {
            scenario: "rt_test".to_string(),
            reproducibility_key: "suite:v1:rt_test:0".to_string(),
            captured_at_ms: 1000,
            executed_resize_events: 0,
            events: vec![],
        };
        let json = serde_json::to_string(&timeline).unwrap();
        let back: ResizeTimeline = serde_json::from_str(&json).unwrap();
        assert_eq!(timeline, back);
    }

    #[test]
    fn resize_timeline_event_serde_json_roundtrip() {
        let event = ResizeTimelineEvent {
            event_index: 3,
            resize_transaction_id: "key:3".to_string(),
            pane_id: 1,
            tab_id: 2,
            sequence_no: 3,
            action: EventAction::Resize,
            scheduler_decision: "dequeue_latest_intent".to_string(),
            frame_id: 3,
            test_case_id: "roundtrip".to_string(),
            queue_wait_ms: 0,
            reflow_ms: 0,
            render_ms: 0,
            present_ms: 0,
            scheduled_at_ns: 1_000_000_000,
            dispatch_offset_ns: 1_000_100_000,
            total_duration_ns: 5000,
            stages: vec![],
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: ResizeTimelineEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: ExpectationKind serde
    // -----------------------------------------------------------------------

    #[test]
    fn expectation_event_kind_yaml_roundtrip() {
        let yaml = r#"
event:
  event: usage_limit
  detected_at: "~5s"
"#;
        let exp: Expectation = serde_yaml::from_str(yaml).unwrap();
        match &exp.kind {
            ExpectationKind::Event { event, detected_at } => {
                assert_eq!(event, "usage_limit");
                assert_eq!(detected_at.as_deref(), Some("~5s"));
            }
            _ => panic!("Expected Event kind"),
        }
    }

    #[test]
    fn expectation_workflow_kind_yaml_roundtrip() {
        let yaml = "
workflow:
  workflow: handle_usage
";
        let exp: Expectation = serde_yaml::from_str(yaml).unwrap();
        match &exp.kind {
            ExpectationKind::Workflow {
                workflow,
                started_at,
            } => {
                assert_eq!(workflow, "handle_usage");
                assert!(started_at.is_none());
            }
            _ => panic!("Expected Workflow kind"),
        }
    }

    #[test]
    fn expectation_contains_kind_yaml_roundtrip() {
        let yaml = r#"
contains:
  pane: 42
  text: "hello world"
"#;
        let exp: Expectation = serde_yaml::from_str(yaml).unwrap();
        match &exp.kind {
            ExpectationKind::Contains { pane, text } => {
                assert_eq!(*pane, 42);
                assert_eq!(text, "hello world");
            }
            _ => panic!("Expected Contains kind"),
        }
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: Scenario Debug/Clone
    // -----------------------------------------------------------------------

    #[test]
    fn scenario_debug_contains_name() {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let dbg = format!("{:?}", scenario);
        assert!(dbg.contains("basic_test"));
    }

    #[test]
    fn scenario_clone_is_independent() {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mut cloned = scenario.clone();
        cloned.name = "modified".to_string();
        assert_eq!(scenario.name, "basic_test");
        assert_eq!(cloned.name, "modified");
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: to_mock_event edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn to_mock_event_append_empty_content() {
        let event = ScenarioEvent {
            at: Duration::from_secs(0),
            pane: 0,
            action: EventAction::Append,
            content: String::new(),
            name: String::new(),
            comment: None,
        };
        let mock_event = Scenario::to_mock_event(&event).unwrap();
        assert!(matches!(mock_event, MockEvent::AppendOutput(ref s) if s.is_empty()));
    }

    #[test]
    fn to_mock_event_set_title_empty_content() {
        let event = ScenarioEvent {
            at: Duration::from_secs(0),
            pane: 0,
            action: EventAction::SetTitle,
            content: String::new(),
            name: String::new(),
            comment: None,
        };
        let mock_event = Scenario::to_mock_event(&event).unwrap();
        assert!(matches!(mock_event, MockEvent::SetTitle(ref s) if s.is_empty()));
    }

    #[test]
    fn to_mock_event_font_size_trims_whitespace() {
        let event = ScenarioEvent {
            at: Duration::from_secs(0),
            pane: 0,
            action: EventAction::SetFontSize,
            content: "  1.50  ".to_string(),
            name: String::new(),
            comment: None,
        };
        let mock_event = Scenario::to_mock_event(&event).unwrap();
        assert!(matches!(mock_event, MockEvent::AppendOutput(ref s) if s == "[FONT_SIZE:1.50]"));
    }

    #[test]
    fn to_mock_event_typing_and_paste_emit_marked_output() {
        let typing = ScenarioEvent {
            at: Duration::from_secs(0),
            pane: 0,
            action: EventAction::Typing,
            content: "hello".to_string(),
            name: String::new(),
            comment: None,
        };
        let paste = ScenarioEvent {
            at: Duration::from_secs(0),
            pane: 0,
            action: EventAction::Paste,
            content: "world".to_string(),
            name: String::new(),
            comment: None,
        };
        let typing_event = Scenario::to_mock_event(&typing).unwrap();
        let paste_event = Scenario::to_mock_event(&paste).unwrap();
        assert!(matches!(typing_event, MockEvent::AppendOutput(ref s) if s == "[TYPING:hello]"));
        assert!(matches!(paste_event, MockEvent::AppendOutput(ref s) if s == "[PASTE:world]"));
    }

    #[test]
    fn to_mock_event_mouse_prefers_name_then_content() {
        let by_name = ScenarioEvent {
            at: Duration::from_secs(0),
            pane: 0,
            action: EventAction::Mouse,
            content: "ignored".to_string(),
            name: "right_click".to_string(),
            comment: None,
        };
        let by_content = ScenarioEvent {
            at: Duration::from_secs(0),
            pane: 0,
            action: EventAction::Mouse,
            content: "wheel_up".to_string(),
            name: String::new(),
            comment: None,
        };
        let event_name = Scenario::to_mock_event(&by_name).unwrap();
        let event_content = Scenario::to_mock_event(&by_content).unwrap();
        assert!(matches!(event_name, MockEvent::AppendOutput(ref s) if s == "[MOUSE:right_click]"));
        assert!(matches!(event_content, MockEvent::AppendOutput(ref s) if s == "[MOUSE:wheel_up]"));
    }

    #[test]
    fn to_mock_event_marker_with_empty_name() {
        let event = ScenarioEvent {
            at: Duration::from_secs(0),
            pane: 0,
            action: EventAction::Marker,
            content: String::new(),
            name: String::new(),
            comment: None,
        };
        let mock_event = Scenario::to_mock_event(&event).unwrap();
        assert!(matches!(mock_event, MockEvent::AppendOutput(ref s) if s == "[MARKER:]"));
    }

    #[test]
    fn to_mock_event_resize_with_whitespace_in_spec() {
        let event = ScenarioEvent {
            at: Duration::from_secs(0),
            pane: 0,
            action: EventAction::Resize,
            content: " 100 x 50 ".to_string(),
            name: String::new(),
            comment: None,
        };
        let mock_event = Scenario::to_mock_event(&event).unwrap();
        assert!(matches!(mock_event, MockEvent::Resize(100, 50)));
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: ScenarioPane defaults
    // -----------------------------------------------------------------------

    #[test]
    fn scenario_pane_defaults_match_default_functions() {
        assert_eq!(default_title(), "pane");
        assert_eq!(default_domain(), "local");
        assert_eq!(default_cwd(), "/home/user");
        assert_eq!(default_cols(), 80);
        assert_eq!(default_rows(), 24);
        assert_eq!(default_window_id(), 0);
        assert_eq!(default_tab_id(), 0);
    }

    #[test]
    fn scenario_pane_with_custom_window_and_tab() {
        let yaml = r#"
name: custom_ids
duration: "1s"
panes:
  - id: 5
    window_id: 10
    tab_id: 20
    cols: 132
    rows: 50
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let pane = &scenario.panes[0];
        assert_eq!(pane.id, 5);
        assert_eq!(pane.window_id, 10);
        assert_eq!(pane.tab_id, 20);
        assert_eq!(pane.cols, 132);
        assert_eq!(pane.rows, 50);
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: duration_ns_u64 and epoch_ms_u64 helpers
    // -----------------------------------------------------------------------

    #[test]
    fn duration_ns_u64_zero() {
        assert_eq!(duration_ns_u64(Duration::ZERO), 0);
    }

    #[test]
    fn duration_ns_u64_one_second() {
        assert_eq!(duration_ns_u64(Duration::from_secs(1)), 1_000_000_000);
    }

    #[test]
    fn duration_ns_u64_sub_nanosecond_precision() {
        assert_eq!(duration_ns_u64(Duration::from_nanos(42)), 42);
    }

    #[test]
    fn epoch_ms_u64_returns_nonzero() {
        // Current time should be well past epoch
        assert!(epoch_ms_u64() > 0);
    }

    #[test]
    fn epoch_ms_u64_reasonable_range() {
        // Should be after 2020-01-01 (epoch ms ~ 1577836800000)
        let ms = epoch_ms_u64();
        assert!(ms > 1_577_836_800_000);
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: Async execution edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn execute_until_exact_boundary() {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        // Execute exactly at 1s boundary (first event is at 1s)
        let count = scenario
            .execute_until(&mock, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn execute_until_just_before_first_event() {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let count = scenario
            .execute_until(&mock, Duration::from_millis(999))
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn execute_until_far_future() {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        // Way past all events
        let count = scenario
            .execute_until(&mock, Duration::from_secs(9999))
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn setup_pane_0_is_active() {
        let yaml = r#"
name: active_test
duration: "1s"
panes:
  - id: 0
  - id: 5
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let p0 = mock.pane_state(0).await.unwrap();
        assert!(p0.is_active);
        let p5 = mock.pane_state(5).await.unwrap();
        assert!(!p5.is_active);
    }

    #[tokio::test]
    async fn setup_panes_not_zoomed() {
        let yaml = r#"
name: zoom_test
duration: "1s"
panes:
  - id: 0
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let p0 = mock.pane_state(0).await.unwrap();
        assert!(!p0.is_zoomed);
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: resize timeline with partial execution
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn execute_until_with_resize_timeline_partial() {
        let yaml = r#"
name: partial_resize
duration: "10s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: resize
    content: "100x30"
  - at: "5s"
    pane: 0
    action: resize
    content: "120x40"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        // Only up to 3s: first resize but not second
        let (count, timeline) = scenario
            .execute_until_with_resize_timeline(&mock, Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(timeline.executed_resize_events, 1);
        assert_eq!(timeline.events.len(), 1);
        assert_eq!(timeline.events[0].action, EventAction::Resize);
    }

    #[tokio::test]
    async fn execute_until_with_resize_timeline_no_resize_events() {
        let yaml = r#"
name: no_resize
duration: "5s"
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
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let (count, timeline) = scenario
            .execute_all_with_resize_timeline(&mock)
            .await
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(timeline.executed_resize_events, 0);
        assert!(timeline.events.is_empty());
    }

    #[tokio::test]
    async fn resize_timeline_captured_at_is_recent() {
        let yaml = r#"
name: ts_check
duration: "1s"
panes:
  - id: 0
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let (_count, timeline) = scenario
            .execute_all_with_resize_timeline(&mock)
            .await
            .unwrap();
        // Should be a recent epoch ms
        assert!(timeline.captured_at_ms > 1_577_836_800_000);
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: TutorialSandbox extended
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn sandbox_check_event_expectation_returns_false() {
        let sandbox = TutorialSandbox::new().await;
        // Event expectations always return false (need runtime)
        let result = sandbox
            .check_expectation(&ExpectationKind::Event {
                event: "test".to_string(),
                detected_at: None,
            })
            .await;
        assert!(!result);
    }

    #[tokio::test]
    async fn sandbox_check_workflow_expectation_returns_false() {
        let sandbox = TutorialSandbox::new().await;
        let result = sandbox
            .check_expectation(&ExpectationKind::Workflow {
                workflow: "test".to_string(),
                started_at: None,
            })
            .await;
        assert!(!result);
    }

    #[tokio::test]
    async fn sandbox_check_contains_nonexistent_pane() {
        let sandbox = TutorialSandbox::new().await;
        let result = sandbox
            .check_expectation(&ExpectationKind::Contains {
                pane: 999,
                text: "anything".to_string(),
            })
            .await;
        assert!(!result);
    }

    #[tokio::test]
    async fn sandbox_check_contains_missing_text() {
        let sandbox = TutorialSandbox::new().await;
        let result = sandbox
            .check_expectation(&ExpectationKind::Contains {
                pane: 0,
                text: "this text does not exist".to_string(),
            })
            .await;
        assert!(!result);
    }

    #[tokio::test]
    async fn sandbox_check_contains_present_text() {
        let sandbox = TutorialSandbox::new().await;
        // Pane 0 has initial content "$ "
        let result = sandbox
            .check_expectation(&ExpectationKind::Contains {
                pane: 0,
                text: "$ ".to_string(),
            })
            .await;
        assert!(result);
    }

    #[tokio::test]
    async fn sandbox_indicator_toggle() {
        let mut sandbox = TutorialSandbox::new().await;
        assert_eq!(sandbox.format_output("x"), "[SANDBOX] x");
        sandbox.set_show_indicator(false);
        assert_eq!(sandbox.format_output("x"), "x");
        sandbox.set_show_indicator(true);
        assert_eq!(sandbox.format_output("x"), "[SANDBOX] x");
    }

    #[tokio::test]
    async fn sandbox_command_log_timestamps_are_monotonic() {
        let mut sandbox = TutorialSandbox::new().await;
        sandbox.log_command("cmd1", None);
        sandbox.log_command("cmd2", None);
        sandbox.log_command("cmd3", None);

        let log = sandbox.command_log();
        assert_eq!(log.len(), 3);
        // Timestamps should be non-decreasing
        assert!(log[0].timestamp_ms <= log[1].timestamp_ms);
        assert!(log[1].timestamp_ms <= log[2].timestamp_ms);
    }

    #[tokio::test]
    async fn sandbox_format_output_empty_text() {
        let sandbox = TutorialSandbox::new().await;
        assert_eq!(sandbox.format_output(""), "[SANDBOX] ");
    }

    #[tokio::test]
    async fn sandbox_with_expectations_mixed_types() {
        let yaml = r#"
name: mixed_exp
duration: "5s"
panes:
  - id: 0
    initial_content: "present text"
events: []
expectations:
  - contains:
      pane: 0
      text: "present text"
  - event:
      event: some_event
  - workflow:
      workflow: some_workflow
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let sandbox = TutorialSandbox::with_scenario(scenario).await.unwrap();

        let (pass, fail, skip) = sandbox.check_all_expectations().await;
        assert_eq!(pass, 1); // contains passes
        assert_eq!(fail, 0);
        assert_eq!(skip, 2); // event and workflow are skipped
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: SandboxCommand serialization
    // -----------------------------------------------------------------------

    #[test]
    fn sandbox_command_serialize() {
        let cmd = SandboxCommand {
            command: "ft status".to_string(),
            timestamp_ms: 12345,
            exercise_id: Some("ex1".to_string()),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("ft status"));
        assert!(json.contains("12345"));
        assert!(json.contains("ex1"));
    }

    #[test]
    fn sandbox_command_serialize_no_exercise() {
        let cmd = SandboxCommand {
            command: "ls".to_string(),
            timestamp_ms: 0,
            exercise_id: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("null"));
    }

    #[test]
    fn sandbox_command_debug() {
        let cmd = SandboxCommand {
            command: "test".to_string(),
            timestamp_ms: 100,
            exercise_id: None,
        };
        let dbg = format!("{:?}", cmd);
        assert!(dbg.contains("test"));
        assert!(dbg.contains("100"));
    }

    #[test]
    fn sandbox_command_clone() {
        let cmd = SandboxCommand {
            command: "original".to_string(),
            timestamp_ms: 42,
            exercise_id: Some("e1".to_string()),
        };
        let cloned = cmd.clone();
        assert_eq!(cmd.command, cloned.command);
        assert_eq!(cmd.timestamp_ms, cloned.timestamp_ms);
        assert_eq!(cmd.exercise_id, cloned.exercise_id);
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: Scenario YAML file I/O edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn scenario_load_from_temp_file_with_metadata() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.yaml");
        let yaml = r#"
name: file_meta
duration: "5s"
metadata:
  suite: file_suite
  suite_version: v2
  seed: "99"
panes:
  - id: 0
events: []
"#;
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "{}", yaml).unwrap();
        drop(f);

        let scenario = Scenario::load(&path).unwrap();
        assert_eq!(scenario.reproducibility_key(), "file_suite:v2:file_meta:99");
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: ScenarioEvent Debug/Clone
    // -----------------------------------------------------------------------

    #[test]
    fn scenario_event_debug_format() {
        let event = ScenarioEvent {
            at: Duration::from_secs(5),
            pane: 3,
            action: EventAction::Clear,
            content: String::new(),
            name: String::new(),
            comment: Some("a comment".to_string()),
        };
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("Clear"));
        assert!(dbg.contains("a comment"));
    }

    #[test]
    fn scenario_event_clone() {
        let event = ScenarioEvent {
            at: Duration::from_secs(1),
            pane: 0,
            action: EventAction::Append,
            content: "hello".to_string(),
            name: "marker".to_string(),
            comment: None,
        };
        let cloned = event.clone();
        assert_eq!(cloned.at, event.at);
        assert_eq!(cloned.pane, event.pane);
        assert_eq!(cloned.action, event.action);
        assert_eq!(cloned.content, event.content);
        assert_eq!(cloned.name, event.name);
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: ResizeQueueMetrics edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn resize_queue_metrics_zero_depth() {
        let m = ResizeQueueMetrics {
            depth_before: 0,
            depth_after: 0,
        };
        assert_eq!(m.depth_before, 0);
        assert_eq!(m.depth_after, 0);
    }

    #[test]
    fn resize_queue_metrics_clone_eq() {
        let m1 = ResizeQueueMetrics {
            depth_before: 5,
            depth_after: 4,
        };
        let m2 = m1.clone();
        assert_eq!(m1, m2);
    }

    #[test]
    fn resize_queue_metrics_debug() {
        let m = ResizeQueueMetrics {
            depth_before: 10,
            depth_after: 9,
        };
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("10"));
        assert!(dbg.contains("9"));
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: ResizeTimelineStageSummary
    // -----------------------------------------------------------------------

    #[test]
    fn resize_timeline_stage_summary_debug() {
        let s = ResizeTimelineStageSummary {
            stage: ResizeTimelineStage::RenderPrep,
            samples: 10,
            total_duration_ns: 5000,
            avg_duration_ns: 500.0,
            p50_duration_ns: 400,
            p95_duration_ns: 900,
            p99_duration_ns: 950,
            max_duration_ns: 1000,
        };
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("RenderPrep"));
        assert!(dbg.contains("5000"));
    }

    #[test]
    fn resize_timeline_stage_summary_clone() {
        let s = ResizeTimelineStageSummary {
            stage: ResizeTimelineStage::Presentation,
            samples: 1,
            total_duration_ns: 100,
            avg_duration_ns: 100.0,
            p50_duration_ns: 100,
            p95_duration_ns: 100,
            p99_duration_ns: 100,
            max_duration_ns: 100,
        };
        let c = s.clone();
        assert_eq!(s.stage, c.stage);
        assert_eq!(s.samples, c.samples);
        assert_eq!(s.total_duration_ns, c.total_duration_ns);
    }

    // -----------------------------------------------------------------------
    // NEW TESTS: Scenario description default
    // -----------------------------------------------------------------------

    #[test]
    fn scenario_description_defaults_to_empty() {
        let yaml = r#"
name: no_desc
duration: "1s"
panes: []
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        assert!(scenario.description.is_empty());
    }

    #[test]
    fn scenario_description_preserved() {
        let yaml = r#"
name: with_desc
description: "My detailed description"
duration: "1s"
panes: []
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        assert_eq!(scenario.description, "My detailed description");
    }
}
