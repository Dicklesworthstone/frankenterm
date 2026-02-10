//! Interactive tutorial engine for wa
//!
//! Provides a guided onboarding experience with:
//! - State machine tracking progress through exercises
//! - Persistent progress storage in `~/.config/wa/learn.json`
//! - CLI integration via `wa learn` commands
//!
//! # Example
//!
//! ```rust,ignore
//! use wa_core::learn::{TutorialEngine, TutorialEvent};
//!
//! let mut engine = TutorialEngine::load_or_create()?;
//! engine.handle_event(TutorialEvent::StartTrack("basics".into()))?;
//! engine.handle_event(TutorialEvent::CompleteExercise("basics.1".into()))?;
//! engine.save()?;
//! ```

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;

use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, instrument, warn};

use crate::environment::DetectedEnvironment;

/// Errors that can occur in the tutorial engine
#[derive(Debug, Error)]
pub enum LearnError {
    #[error("Failed to read progress file: {0}")]
    ReadProgress(#[from] io::Error),

    #[error("Failed to parse progress file: {0}")]
    ParseProgress(#[from] serde_json::Error),

    #[error("Unknown track: {0}")]
    UnknownTrack(String),
}

/// Result type for learn operations
pub type Result<T> = std::result::Result<T, LearnError>;

/// Track identifier (e.g., "basics", "events", "workflows")
pub type TrackId = String;

/// Exercise identifier (e.g., "basics.1", "events.2")
pub type ExerciseId = String;

/// Achievement rarity tier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Rarity {
    Common,
    Uncommon,
    Rare,
    Epic,
}

impl Rarity {
    /// Display label for this rarity
    pub fn label(self) -> &'static str {
        match self {
            Self::Common => "Common",
            Self::Uncommon => "Uncommon",
            Self::Rare => "Rare",
            Self::Epic => "Epic",
        }
    }
}

impl std::fmt::Display for Rarity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Static definition of an achievable milestone
#[derive(Debug, Clone)]
pub struct AchievementDefinition {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub icon: char,
    pub rarity: Rarity,
    /// Secret achievements are hidden until unlocked
    pub secret: bool,
}

/// All built-in achievement definitions
pub const BUILTIN_ACHIEVEMENTS: &[AchievementDefinition] = &[
    // --- Exercise milestones (Common) ---
    AchievementDefinition {
        id: "first_step",
        name: "First Step",
        description: "Completed your very first exercise",
        icon: '\u{1F463}', // footprints
        rarity: Rarity::Common,
        secret: false,
    },
    AchievementDefinition {
        id: "first_watch",
        name: "First Watch",
        description: "Started the wa watcher for the first time",
        icon: '\u{1F440}', // eyes
        rarity: Rarity::Common,
        secret: false,
    },
    AchievementDefinition {
        id: "first_event",
        name: "Event Spotter",
        description: "Viewed your first detected event",
        icon: '\u{1F50D}', // magnifying glass
        rarity: Rarity::Common,
        secret: false,
    },
    AchievementDefinition {
        id: "searcher",
        name: "Data Detective",
        description: "Searched captured pane output with FTS5",
        icon: '\u{1F50E}', // magnifying glass right
        rarity: Rarity::Common,
        secret: false,
    },
    AchievementDefinition {
        id: "workflow_runner",
        name: "Workflow Runner",
        description: "Explored available workflow definitions",
        icon: '\u{2699}', // gear
        rarity: Rarity::Common,
        secret: false,
    },
    // --- Track completions (Uncommon) ---
    AchievementDefinition {
        id: "track_basics_complete",
        name: "Basics Master",
        description: "Completed all Basics exercises",
        icon: '\u{2B50}', // star
        rarity: Rarity::Uncommon,
        secret: false,
    },
    AchievementDefinition {
        id: "track_events_complete",
        name: "Pattern Detective",
        description: "Completed all Events exercises",
        icon: '\u{1F3AF}', // dart
        rarity: Rarity::Uncommon,
        secret: false,
    },
    AchievementDefinition {
        id: "track_workflows_complete",
        name: "Workflow Wizard",
        description: "Completed all Workflows exercises",
        icon: '\u{1FA84}', // magic wand
        rarity: Rarity::Uncommon,
        secret: false,
    },
    AchievementDefinition {
        id: "track_robot_complete",
        name: "Robot Operator",
        description: "Completed all Robot Mode exercises",
        icon: '\u{1F916}', // robot face
        rarity: Rarity::Uncommon,
        secret: false,
    },
    AchievementDefinition {
        id: "track_advanced_complete",
        name: "wa Master",
        description: "Completed all Advanced exercises",
        icon: '\u{1F9D9}', // mage
        rarity: Rarity::Uncommon,
        secret: false,
    },
    // --- Cross-track milestones (Uncommon/Rare) ---
    AchievementDefinition {
        id: "explorer",
        name: "Explorer",
        description: "Completed at least one exercise in every track",
        icon: '\u{1F9ED}', // compass
        rarity: Rarity::Uncommon,
        secret: false,
    },
    AchievementDefinition {
        id: "completionist",
        name: "Completionist",
        description: "Completed every single exercise across all tracks",
        icon: '\u{1F3C6}', // trophy
        rarity: Rarity::Rare,
        secret: false,
    },
    // --- Ultimate (Epic) ---
    AchievementDefinition {
        id: "wa_master",
        name: "wa Master",
        description: "Completed all tracks and mastered wa",
        icon: '\u{1F451}', // crown
        rarity: Rarity::Epic,
        secret: false,
    },
    // --- Secret achievements ---
    AchievementDefinition {
        id: "speed_runner",
        name: "Speed Runner",
        description: "Completed the Basics track in under 3 minutes",
        icon: '\u{26A1}', // lightning
        rarity: Rarity::Rare,
        secret: true,
    },
    AchievementDefinition {
        id: "night_owl",
        name: "Night Owl",
        description: "Completed an exercise after midnight",
        icon: '\u{1F989}', // owl
        rarity: Rarity::Rare,
        secret: true,
    },
];

/// Look up an achievement definition by ID
pub fn achievement_definition(id: &str) -> Option<&'static AchievementDefinition> {
    BUILTIN_ACHIEVEMENTS.iter().find(|d| d.id == id)
}

/// Achievement unlocked during tutorial
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Achievement {
    /// Unique achievement ID
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Description of what was accomplished
    pub description: String,
    /// When the achievement was unlocked
    pub unlocked_at: DateTime<Utc>,
}

/// Tutorial progress state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TutorialState {
    /// Schema version for forward compatibility
    pub version: u32,
    /// Currently active track (if any)
    pub current_track: Option<TrackId>,
    /// Currently active exercise within the track
    pub current_exercise: Option<ExerciseId>,
    /// Set of completed exercise IDs
    pub completed_exercises: HashSet<ExerciseId>,
    /// Achievements earned
    pub achievements: Vec<Achievement>,
    /// When the tutorial was first started
    pub started_at: DateTime<Utc>,
    /// Last activity timestamp
    pub last_active: DateTime<Utc>,
    /// Total time spent in tutorial (minutes)
    pub total_time_minutes: u32,
}

impl Default for TutorialState {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            version: 1,
            current_track: None,
            current_exercise: None,
            completed_exercises: HashSet::new(),
            achievements: Vec::new(),
            started_at: now,
            last_active: now,
            total_time_minutes: 0,
        }
    }
}

/// Events that can modify tutorial state
#[derive(Debug, Clone)]
pub enum TutorialEvent {
    /// Start or resume a specific track
    StartTrack(TrackId),
    /// Mark an exercise as completed
    CompleteExercise(ExerciseId),
    /// Skip an exercise (marks as seen but not completed)
    SkipExercise(ExerciseId),
    /// Unlock an achievement
    UnlockAchievement {
        id: String,
        name: String,
        description: String,
    },
    /// Reset all progress
    Reset,
    /// Update activity timestamp
    Heartbeat,
}

/// Track definition with exercises
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub name: String,
    pub description: String,
    pub estimated_minutes: u32,
    pub exercises: Vec<Exercise>,
}

/// Environment requirement for an exercise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    /// WezTerm must be running and reachable via CLI.
    WeztermRunning,
    /// At least one agent pane must be detected.
    AgentPresent,
    /// The wa watcher daemon must be running.
    WatcherRunning,
    /// The wa database must contain data (segments/events).
    DbHasData,
    /// wa configuration (wa.toml) must exist.
    WaConfigured,
}

/// Result of checking whether an exercise can run in the current environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanRun {
    /// All requirements are satisfied.
    Yes,
    /// Requirements not met but exercise supports sandbox/simulation mode.
    Simulation(&'static str),
    /// Requirements not met and exercise cannot be simulated.
    No(&'static str),
}

/// Single exercise within a track
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exercise {
    pub id: ExerciseId,
    pub title: String,
    pub description: String,
    pub instructions: Vec<String>,
    /// Command to verify completion (optional)
    pub verification_command: Option<String>,
    /// Expected output pattern for verification
    pub verification_pattern: Option<String>,
    /// Environment requirements for this exercise
    #[serde(default)]
    pub requirements: Vec<Requirement>,
    /// Whether the exercise can run in sandbox/simulation mode when requirements aren't met
    #[serde(default)]
    pub can_simulate: bool,
}

/// Tutorial engine managing state and progress
pub struct TutorialEngine {
    state: TutorialState,
    tracks: Vec<Track>,
    progress_path: PathBuf,
}

impl TutorialEngine {
    /// Default progress file location
    pub fn default_progress_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("wa")
            .join("learn.json")
    }

    /// Load existing progress or create fresh state
    #[instrument(skip_all, level = "debug")]
    pub fn load_or_create() -> Result<Self> {
        let progress_path = Self::default_progress_path();
        Self::load_or_create_at(progress_path)
    }

    /// Load or create with custom progress path
    pub fn load_or_create_at(progress_path: PathBuf) -> Result<Self> {
        let state = if progress_path.exists() {
            debug!(?progress_path, "Loading existing tutorial progress");
            let contents = fs::read_to_string(&progress_path)?;
            serde_json::from_str(&contents)?
        } else {
            debug!(?progress_path, "Creating new tutorial progress");
            TutorialState::default()
        };

        let tracks = Self::load_builtin_tracks();

        Ok(Self {
            state,
            tracks,
            progress_path,
        })
    }

    /// Load built-in track definitions
    fn load_builtin_tracks() -> Vec<Track> {
        vec![
            Track {
                id: "basics".into(),
                name: "Basics".into(),
                description: "What is wa? Check WezTerm. Start watching. View status. See events."
                    .into(),
                estimated_minutes: 5,
                exercises: vec![
                    Exercise {
                        id: "basics.1".into(),
                        title: "What is wa?".into(),
                        description:
                            "Get the 30-second mental model: wa is a terminal hypervisor for AI agent swarms."
                                .into(),
                        instructions: vec![
                            "Read this flow: WezTerm panes -> ingest -> storage/events -> workflows + robot/MCP.".into(),
                            "Goal: understand wa observes output, detects state transitions, then safely automates responses.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "basics.2".into(),
                        title: "Check WezTerm".into(),
                        description: "Verify wa can see your WezTerm environment".into(),
                        instructions: vec![
                            "Run: wa doctor".into(),
                            "Confirm WezTerm is detected (or use sandbox mode if unavailable).".into(),
                        ],
                        verification_command: Some("wa doctor --json".into()),
                        verification_pattern: Some("wezterm".into()),
                        requirements: vec![],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "basics.3".into(),
                        title: "Start the watcher".into(),
                        description: "Launch the wa daemon to observe terminal activity".into(),
                        instructions: vec![
                            "Run: wa watch".into(),
                            "The watcher starts in the background".into(),
                        ],
                        verification_command: Some("wa status --json".into()),
                        verification_pattern: Some("\"running\"\\s*:\\s*true".into()),
                        requirements: vec![Requirement::WeztermRunning],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "basics.4".into(),
                        title: "View pane status".into(),
                        description: "See what panes wa is observing".into(),
                        instructions: vec![
                            "Run: wa status".into(),
                            "Run: wa list".into(),
                            "Check that pane metadata and watcher state make sense.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::WatcherRunning],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "basics.5".into(),
                        title: "Your first event".into(),
                        description:
                            "Inspect detections and understand what wa records when patterns match."
                                .into(),
                        instructions: vec![
                            "Run: wa events".into(),
                            "If there are no events yet, continue in sandbox/simulation mode.".into(),
                            "Identify one event type and what it means.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::DbHasData],
                        can_simulate: true,
                    },
                ],
            },
            Track {
                id: "events".into(),
                name: "Events".into(),
                description: "Understanding detections and pattern matching.".into(),
                estimated_minutes: 10,
                exercises: vec![
                    Exercise {
                        id: "events.1".into(),
                        title: "What are events?".into(),
                        description:
                            "Events are meaningful terminal occurrences wa detects via pattern rules."
                                .into(),
                        instructions: vec![
                            "Review: events represent things like usage limits, compaction, and errors.".into(),
                            "Goal: understand events are structured detections, not raw log lines.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "events.2".into(),
                        title: "Pattern packs".into(),
                        description: "Explore built-in rule packs and what they cover".into(),
                        instructions: vec![
                            "Run: wa rules list".into(),
                            "Identify at least one core pack and an example rule it contains.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "events.3".into(),
                        title: "View recent events".into(),
                        description: "Inspect recent detections and event fields".into(),
                        instructions: vec![
                            "Run: wa events --limit 5".into(),
                            "Inspect fields like rule/event type, pane, and timestamp.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::DbHasData],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "events.4".into(),
                        title: "Search events and output".into(),
                        description: "Use FTS queries to find relevant event context".into(),
                        instructions: vec![
                            "Run: wa query \"compaction\"".into(),
                            "Try at least one alternate search term and compare results.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::DbHasData],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "events.5".into(),
                        title: "Test a pattern".into(),
                        description: "Run rule evaluation against sample text".into(),
                        instructions: vec![
                            "Run: wa rules test \"Session limit reached\"".into(),
                            "Observe which rule matches and why.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "events.6".into(),
                        title: "Trigger a detection (simulated)".into(),
                        description:
                            "Simulate a detection path and confirm it appears in the events feed."
                                .into(),
                        instructions: vec![
                            "In sandbox mode, simulate a usage-limit style event.".into(),
                            "Run: wa events and confirm the simulated detection is visible.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::DbHasData],
                        can_simulate: true,
                    },
                ],
            },
            Track {
                id: "workflows".into(),
                name: "Workflows".into(),
                description: "Automating responses to events.".into(),
                estimated_minutes: 15,
                exercises: vec![
                    Exercise {
                        id: "workflows.1".into(),
                        title: "What are workflows?".into(),
                        description:
                            "Workflows are automated multi-step responses to detected events."
                                .into(),
                        instructions: vec![
                            "Review this flow: Event -> Workflow -> Steps -> Verification.".into(),
                            "Goal: understand workflows orchestrate safe, deterministic actions."
                                .into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "workflows.2".into(),
                        title: "Built-in workflows".into(),
                        description: "List available built-in workflows and what they do".into(),
                        instructions: vec![
                            "Run: wa workflow list".into(),
                            "Identify at least one workflow (for example handle_compaction).".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::WaConfigured],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "workflows.3".into(),
                        title: "Policy gates".into(),
                        description:
                            "Understand allow/deny/require-approval decisions before actions run."
                                .into(),
                        instructions: vec![
                            "Review why wa may require approval for risky actions.".into(),
                            "Goal: understand safety checks are intentional guardrails.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "workflows.4".into(),
                        title: "Run a workflow (dry-run)".into(),
                        description: "Preview a workflow step plan without executing side effects"
                            .into(),
                        instructions: vec![
                            "Run: wa workflow run handle_compaction --dry-run".into(),
                            "Inspect the generated steps and verification expectations.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::WaConfigured, Requirement::WatcherRunning],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "workflows.5".into(),
                        title: "Workflow step logs".into(),
                        description: "Inspect workflow execution states, timing, and outcomes".into(),
                        instructions: vec![
                            "Run: wa workflow status <execution_id> -v".into(),
                            "Read step-level status and timing output.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::DbHasData],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "workflows.6".into(),
                        title: "Watch a workflow execute (simulated)".into(),
                        description:
                            "Observe a simulated event-driven workflow execution from trigger to completion."
                                .into(),
                        instructions: vec![
                            "In sandbox mode, simulate an event that triggers a workflow.".into(),
                            "Confirm you can follow step progression to completion.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::DbHasData],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "workflows.7".into(),
                        title: "Approval flow".into(),
                        description: "Practice the require-approval path and continuation".into(),
                        instructions: vec![
                            "Run: wa approve <token> in sandbox/tutorial context.".into(),
                            "Confirm the workflow can continue after approval.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::WaConfigured],
                        can_simulate: true,
                    },
                ],
            },
            Track {
                id: "robot".into(),
                name: "Robot Mode".into(),
                description: "Building machine-readable integrations for agents.".into(),
                estimated_minutes: 10,
                exercises: vec![
                    Exercise {
                        id: "robot.1".into(),
                        title: "What is Robot Mode?".into(),
                        description:
                            "Robot mode provides stable machine-readable output for agent integrations."
                                .into(),
                        instructions: vec![
                            "Review: Robot mode uses a stable success/data/error envelope for automation."
                                .into(),
                            "Goal: understand Robot mode is designed for agent-to-agent tooling.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "robot.2".into(),
                        title: "JSON envelope".into(),
                        description: "Inspect Robot mode state output and envelope structure".into(),
                        instructions: vec![
                            "Run: wa robot state".into(),
                            "Identify the envelope fields (ok/data/error/hint).".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::WeztermRunning],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "robot.3".into(),
                        title: "Error codes".into(),
                        description:
                            "Trigger and inspect a structured Robot mode error response.".into(),
                        instructions: vec![
                            "Run a command with an invalid pane id to observe structured errors."
                                .into(),
                            "Note the error code and hint fields.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "robot.4".into(),
                        title: "Quick-start for agents".into(),
                        description:
                            "Use quick-start output to bootstrap an automated agent session.".into(),
                        instructions: vec![
                            "Run: wa robot quick-start".into(),
                            "Confirm the output contains concise machine-friendly startup context."
                                .into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "robot.5".into(),
                        title: "Poll for unhandled events".into(),
                        description:
                            "Practice the agent loop pattern: poll events -> process -> mark handled."
                                .into(),
                        instructions: vec![
                            "Run: wa robot events --unhandled".into(),
                            "Interpret the returned envelope and event list.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::DbHasData],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "robot.6".into(),
                        title: "Safe send (simulated)".into(),
                        description:
                            "Exercise policy-gated send behavior in sandbox/simulation mode.".into(),
                        instructions: vec![
                            "In sandbox mode, run a Robot send command and inspect policy decision output."
                                .into(),
                            "Confirm unsafe sends are gated instead of silently executed.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::WaConfigured],
                        can_simulate: true,
                    },
                ],
            },
            Track {
                id: "advanced".into(),
                name: "Advanced".into(),
                description:
                    "Custom patterns, multi-agent coordination, and power-user techniques."
                        .into(),
                estimated_minutes: 20,
                exercises: vec![
                    Exercise {
                        id: "advanced.1".into(),
                        title: "Custom pattern basics".into(),
                        description:
                            "Pattern packs define detection rules. User packs extend built-in packs with org-specific patterns."
                                .into(),
                        instructions: vec![
                            "Review: pattern packs contain rules with anchors (fast string match) and optional regex (extraction).".into(),
                            "User packs live in ~/.config/wa/patterns/ or .wa/patterns/ and follow the same format.".into(),
                            "Goal: understand the pack format (name, version, rules[]) and rule fields (id, anchors, regex, severity).".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "advanced.2".into(),
                        title: "Create a pattern rule".into(),
                        description:
                            "Write a custom pattern rule in TOML and save it as a user pack."
                                .into(),
                        instructions: vec![
                            "Create ~/.config/wa/patterns/my-rules.toml with a rule definition.".into(),
                            "Example rule: id=\"myorg.deploy_alert\", anchors=[\"[DEPLOY]\"], severity=\"warning\".".into(),
                            "Run: wa rules list -- your custom rule should appear.".into(),
                        ],
                        verification_command: Some("wa rules list".into()),
                        verification_pattern: Some("myorg\\.".into()),
                        requirements: vec![Requirement::WaConfigured],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "advanced.3".into(),
                        title: "Test custom pattern".into(),
                        description: "Verify your custom rule matches expected text.".into(),
                        instructions: vec![
                            "Run: wa rules test \"[DEPLOY] Production deployment started\"".into(),
                            "Confirm your custom rule is listed in the matches.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::WaConfigured],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "advanced.4".into(),
                        title: "Multi-pane overview".into(),
                        description:
                            "wa monitors all WezTerm panes simultaneously, correlating events across agents."
                                .into(),
                        instructions: vec![
                            "Review: each pane has an independent capture stream and event history.".into(),
                            "Run: wa status -- observe per-pane health and detection counts.".into(),
                            "Goal: understand how wa tracks multiple agents in parallel.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "advanced.5".into(),
                        title: "Event correlation".into(),
                        description:
                            "Events across panes can be correlated by timestamp to understand cross-agent interactions."
                                .into(),
                        instructions: vec![
                            "Run: wa timeline -- view interleaved events from all panes.".into(),
                            "Note how events from different agents are ordered chronologically.".into(),
                            "Goal: use timeline to diagnose cascading failures across agents.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::DbHasData],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "advanced.6".into(),
                        title: "FTS power queries".into(),
                        description:
                            "Use advanced full-text search with boolean operators to find specific output."
                                .into(),
                        instructions: vec![
                            "Run: wa query \"error AND codex NOT timeout\"".into(),
                            "Try prefix matching: wa query \"deploy*\"".into(),
                            "Goal: master FTS5 boolean syntax for precise searches.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::DbHasData],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "advanced.7".into(),
                        title: "Export and analysis".into(),
                        description:
                            "Export captured data for offline analysis or integration with other tools."
                                .into(),
                        instructions: vec![
                            "Run: wa export --format jsonl".into(),
                            "Each line is a self-contained JSON record suitable for jq, pandas, or log aggregators.".into(),
                            "Goal: integrate wa data into your existing observability pipeline.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::DbHasData],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "advanced.8".into(),
                        title: "Explainability with wa why".into(),
                        description:
                            "Ask wa to explain its decisions: why a detection fired, why a workflow ran."
                                .into(),
                        instructions: vec![
                            "Run: wa why <event-id> -- to trace a detection back to its rule and workflow.".into(),
                            "Review the explain output: matched rule, anchors hit, regex groups, workflow steps.".into(),
                            "Goal: debug unexpected detections or missing matches.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![Requirement::DbHasData],
                        can_simulate: true,
                    },
                    Exercise {
                        id: "advanced.9".into(),
                        title: "Track completion".into(),
                        description:
                            "Review everything you have learned across all five tracks.".into(),
                        instructions: vec![
                            "Run: wa learn --status -- review your progress across all tracks.".into(),
                            "Run: wa learn --achievements -- see your achievement collection.".into(),
                            "You have mastered wa: watching, detection, workflows, robot mode, and advanced techniques.".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                        requirements: vec![],
                        can_simulate: true,
                    },
                ],
            },
        ]
    }

    /// Handle a tutorial event, updating state
    #[instrument(skip(self), level = "debug")]
    pub fn handle_event(&mut self, event: TutorialEvent) -> Result<()> {
        let now = Utc::now();

        match event {
            TutorialEvent::StartTrack(track_id) => {
                info!(%track_id, "Starting track");
                // Only update state if track exists
                if let Some(track) = self.tracks.iter().find(|t| t.id == track_id) {
                    self.state.current_track = Some(track_id.clone());
                    // Find first incomplete exercise in track
                    let first_incomplete = track
                        .exercises
                        .iter()
                        .find(|e| !self.state.completed_exercises.contains(&e.id));
                    self.state.current_exercise = first_incomplete.map(|e| e.id.clone());
                    self.state.last_active = now;
                } else {
                    warn!(%track_id, "Attempted to start unknown track");
                }
            }

            TutorialEvent::CompleteExercise(exercise_id) => {
                info!(%exercise_id, "Completing exercise");
                self.state.completed_exercises.insert(exercise_id.clone());
                self.state.last_active = now;

                // Advance to next exercise if possible
                if let Some(track_id) = &self.state.current_track {
                    if let Some(track) = self.tracks.iter().find(|t| &t.id == track_id) {
                        let current_idx = track.exercises.iter().position(|e| e.id == exercise_id);
                        if let Some(idx) = current_idx {
                            self.state.current_exercise =
                                track.exercises.get(idx + 1).map(|e| e.id.clone());
                        }
                    }
                }

                // Check for achievements
                self.check_achievements();
            }

            TutorialEvent::SkipExercise(exercise_id) => {
                debug!(%exercise_id, "Skipping exercise");
                self.state.last_active = now;
                // Advance to next without marking complete
                if let Some(track_id) = &self.state.current_track {
                    if let Some(track) = self.tracks.iter().find(|t| &t.id == track_id) {
                        let current_idx = track.exercises.iter().position(|e| e.id == exercise_id);
                        if let Some(idx) = current_idx {
                            self.state.current_exercise =
                                track.exercises.get(idx + 1).map(|e| e.id.clone());
                        }
                    }
                }
            }

            TutorialEvent::UnlockAchievement {
                id,
                name,
                description,
            } => {
                if !self.state.achievements.iter().any(|a| a.id == id) {
                    info!(%id, %name, "Unlocking achievement");
                    self.state.achievements.push(Achievement {
                        id,
                        name,
                        description,
                        unlocked_at: now,
                    });
                }
            }

            TutorialEvent::Reset => {
                warn!("Resetting tutorial progress");
                self.state = TutorialState::default();
            }

            TutorialEvent::Heartbeat => {
                // Handle potential clock skew by using max(0, elapsed)
                let elapsed_minutes = (now - self.state.last_active).num_minutes();
                if elapsed_minutes > 0 && elapsed_minutes < 60 {
                    // Only count if positive and less than an hour gap
                    // Cap at 5 minutes per heartbeat, use saturating_add to prevent overflow
                    let to_add = (elapsed_minutes as u32).min(5);
                    self.state.total_time_minutes =
                        self.state.total_time_minutes.saturating_add(to_add);
                }
                self.state.last_active = now;
            }
        }

        Ok(())
    }

    /// Check and unlock any earned achievements
    fn check_achievements(&mut self) {
        // Collect achievements to add (avoiding borrow issues)
        let mut to_add: Vec<(String, String, String)> = Vec::new();

        let has = |id: &str| self.state.achievements.iter().any(|a| a.id == id);

        // First step — any exercise completed
        if !self.state.completed_exercises.is_empty() && !has("first_step") {
            to_add.push((
                "first_step".into(),
                "First Step".into(),
                "Completed your very first exercise".into(),
            ));
        }

        // First watch achievement
        if self.state.completed_exercises.contains("basics.3") && !has("first_watch") {
            to_add.push((
                "first_watch".into(),
                "First Watch".into(),
                "Started the wa watcher for the first time".into(),
            ));
        }

        // First event achievement
        if (self.state.completed_exercises.contains("basics.5")
            || self.state.completed_exercises.contains("events.3"))
            && !has("first_event")
        {
            to_add.push((
                "first_event".into(),
                "Event Spotter".into(),
                "Viewed your first detected event".into(),
            ));
        }

        // Searcher — FTS5 search exercise
        if self.state.completed_exercises.contains("events.4") && !has("searcher") {
            to_add.push((
                "searcher".into(),
                "Data Detective".into(),
                "Searched captured pane output with FTS5".into(),
            ));
        }

        // Workflow runner — listed workflows
        if self.state.completed_exercises.contains("workflows.2") && !has("workflow_runner") {
            to_add.push((
                "workflow_runner".into(),
                "Workflow Runner".into(),
                "Explored available workflow definitions".into(),
            ));
        }

        // Track completion achievements
        for track in &self.tracks {
            let all_complete = track
                .exercises
                .iter()
                .all(|e| self.state.completed_exercises.contains(&e.id));
            let achievement_id = format!("track_{}_complete", track.id);

            if all_complete && !has(&achievement_id) {
                let def = achievement_definition(&achievement_id);
                let name =
                    def.map_or_else(|| format!("{} Master", track.name), |d| d.name.to_string());
                let desc = def.map_or_else(
                    || format!("Completed all {} exercises", track.name),
                    |d| d.description.to_string(),
                );
                to_add.push((achievement_id, name, desc));
            }
        }

        // Explorer — at least one exercise completed in every track
        if !has("explorer") {
            let touched_all = self.tracks.iter().all(|track| {
                track
                    .exercises
                    .iter()
                    .any(|e| self.state.completed_exercises.contains(&e.id))
            });
            if touched_all {
                to_add.push((
                    "explorer".into(),
                    "Explorer".into(),
                    "Completed at least one exercise in every track".into(),
                ));
            }
        }

        // Completionist — every exercise in every track
        if !has("completionist") {
            let all_done = self.tracks.iter().all(|track| {
                track
                    .exercises
                    .iter()
                    .all(|e| self.state.completed_exercises.contains(&e.id))
            });
            if all_done {
                to_add.push((
                    "completionist".into(),
                    "Completionist".into(),
                    "Completed every single exercise across all tracks".into(),
                ));
            }
        }

        // wa Master — all tracks completed (same condition as completionist but Epic tier)
        if !has("wa_master") {
            let all_tracks = self.tracks.iter().all(|t| {
                t.exercises
                    .iter()
                    .all(|e| self.state.completed_exercises.contains(&e.id))
            });
            if all_tracks {
                to_add.push((
                    "wa_master".into(),
                    "wa Master".into(),
                    "Completed all tracks and mastered wa".into(),
                ));
            }
        }

        // Speed runner — basics complete and total time <= 3 minutes (secret)
        if !has("speed_runner") && self.is_track_complete_internal("basics") {
            if self.state.total_time_minutes <= 3 {
                to_add.push((
                    "speed_runner".into(),
                    "Speed Runner".into(),
                    "Completed the Basics track in under 3 minutes".into(),
                ));
            }
        }

        // Night owl — exercise completed after midnight local time (secret)
        // We check the current last_active time hour
        if !has("night_owl") {
            let hour = self.state.last_active.time().hour();
            if hour < 5 {
                to_add.push((
                    "night_owl".into(),
                    "Night Owl".into(),
                    "Completed an exercise after midnight".into(),
                ));
            }
        }

        // Now add the achievements
        for (id, name, description) in to_add {
            let _ = self.handle_event(TutorialEvent::UnlockAchievement {
                id,
                name,
                description,
            });
        }
    }

    /// Internal helper — checks track completion without borrowing &self.tracks
    fn is_track_complete_internal(&self, track_id: &str) -> bool {
        self.tracks
            .iter()
            .find(|t| t.id == track_id)
            .map(|track| {
                track
                    .exercises
                    .iter()
                    .all(|e| self.state.completed_exercises.contains(&e.id))
            })
            .unwrap_or(false)
    }

    /// Save current state to progress file
    #[instrument(skip(self), level = "debug")]
    pub fn save(&self) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.progress_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let contents = serde_json::to_string_pretty(&self.state)?;
        fs::write(&self.progress_path, contents)?;
        debug!(path = ?self.progress_path, "Saved tutorial progress");
        Ok(())
    }

    /// Get current state (immutable)
    pub fn state(&self) -> &TutorialState {
        &self.state
    }

    /// Get all available tracks
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Get a specific track by ID
    pub fn get_track(&self, track_id: &str) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == track_id)
    }

    /// Get current exercise details
    pub fn current_exercise(&self) -> Option<&Exercise> {
        let track_id = self.state.current_track.as_ref()?;
        let exercise_id = self.state.current_exercise.as_ref()?;
        let track = self.get_track(track_id)?;
        track.exercises.iter().find(|e| &e.id == exercise_id)
    }

    /// Check if a track is completed
    pub fn is_track_complete(&self, track_id: &str) -> bool {
        self.get_track(track_id)
            .map(|track| {
                track
                    .exercises
                    .iter()
                    .all(|e| self.state.completed_exercises.contains(&e.id))
            })
            .unwrap_or(false)
    }

    /// Get completion percentage for a track
    pub fn track_progress(&self, track_id: &str) -> (usize, usize) {
        self.get_track(track_id)
            .map(|track| {
                let completed = track
                    .exercises
                    .iter()
                    .filter(|e| self.state.completed_exercises.contains(&e.id))
                    .count();
                (completed, track.exercises.len())
            })
            .unwrap_or((0, 0))
    }

    /// Get overall completion percentage
    pub fn overall_progress(&self) -> (usize, usize) {
        let total: usize = self.tracks.iter().map(|t| t.exercises.len()).sum();
        let completed = self.state.completed_exercises.len();
        (completed, total)
    }

    /// Get the full achievement collection: all definitions with unlock status
    pub fn achievement_collection(&self) -> Vec<AchievementEntry> {
        BUILTIN_ACHIEVEMENTS
            .iter()
            .map(|def| {
                let unlocked = self.state.achievements.iter().find(|a| a.id == def.id);
                AchievementEntry {
                    id: def.id.to_string(),
                    name: def.name.to_string(),
                    description: def.description.to_string(),
                    icon: def.icon,
                    rarity: def.rarity,
                    secret: def.secret,
                    unlocked_at: unlocked.map(|a| a.unlocked_at),
                }
            })
            .collect()
    }

    /// Format a single achievement unlock notification for terminal display
    pub fn format_achievement_unlock(achievement: &Achievement) -> String {
        let def = achievement_definition(&achievement.id);
        let icon = def.map_or('\u{1F3C6}', |d| d.icon);
        let rarity = def.map_or(Rarity::Common, |d| d.rarity);

        let inner_width = 42;
        let top = format!("\u{256D}{}\u{256E}", "\u{2500}".repeat(inner_width));
        let bot = format!("\u{2570}{}\u{256F}", "\u{2500}".repeat(inner_width));
        let blank = format!("\u{2502}{}\u{2502}", " ".repeat(inner_width));

        let header = format!(
            "\u{2502} \u{1F3C6} Achievement Unlocked!{}\u{2502}",
            " ".repeat(inner_width - 24)
        );
        let name_line = format!(" {} {}", icon, achievement.name);
        let name_padded = format!(
            "\u{2502}{}{}\u{2502}",
            name_line,
            " ".repeat(inner_width.saturating_sub(name_line.len()))
        );
        let desc_line = format!(" \"{}\"", achievement.description);
        // Truncate long descriptions (char-boundary safe to avoid panic on multi-byte UTF-8)
        let desc_truncated = if desc_line.len() > inner_width - 1 {
            let mut end = inner_width - 4;
            while end > 0 && !desc_line.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &desc_line[..end])
        } else {
            desc_line.clone()
        };
        let desc_padded = format!(
            "\u{2502}{}{}\u{2502}",
            desc_truncated,
            " ".repeat(inner_width.saturating_sub(desc_truncated.len()))
        );
        let rarity_line = format!(" [{}]", rarity);
        let rarity_padded = format!(
            "\u{2502}{}{}\u{2502}",
            rarity_line,
            " ".repeat(inner_width.saturating_sub(rarity_line.len()))
        );

        format!(
            "{top}\n{header}\n{blank}\n{name_padded}\n{desc_padded}\n{blank}\n{rarity_padded}\n{bot}"
        )
    }

    /// Format the full achievement collection for terminal display
    pub fn format_achievement_list(&self) -> String {
        let collection = self.achievement_collection();
        let unlocked_count = collection
            .iter()
            .filter(|a| a.unlocked_at.is_some())
            .count();
        let total = collection.len();

        let mut lines = Vec::new();
        lines.push(format!("Achievements [{}/{}]\n", unlocked_count, total));

        for entry in &collection {
            if entry.secret && entry.unlocked_at.is_none() {
                lines.push(format!("  \u{2753} ??? — [{}] (secret)", entry.rarity));
            } else if entry.unlocked_at.is_some() {
                lines.push(format!(
                    "  {} {} — {} [{}]",
                    entry.icon, entry.name, entry.description, entry.rarity
                ));
            } else {
                lines.push(format!(
                    "  \u{25CB} {} — {} [{}]",
                    entry.name, entry.description, entry.rarity
                ));
            }
        }

        lines.join("\n")
    }
}

/// Achievement entry combining definition with unlock status (for CLI/JSON output)
#[derive(Debug, Clone, Serialize)]
pub struct AchievementEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(serialize_with = "serialize_char")]
    pub icon: char,
    pub rarity: Rarity,
    pub secret: bool,
    pub unlocked_at: Option<DateTime<Utc>>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde requires &T signature
fn serialize_char<S: serde::Serializer>(c: &char, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&c.to_string())
}

/// Summary for CLI status output
#[derive(Debug, Serialize)]
pub struct TutorialStatus {
    pub current_track: Option<String>,
    pub current_exercise: Option<String>,
    pub completed_exercises: usize,
    pub total_exercises: usize,
    pub achievements_earned: usize,
    pub total_time_minutes: u32,
    pub tracks: Vec<TrackStatus>,
}

#[derive(Debug, Serialize)]
pub struct TrackStatus {
    pub id: String,
    pub name: String,
    pub completed: usize,
    pub total: usize,
    pub is_complete: bool,
}

impl From<&TutorialEngine> for TutorialStatus {
    fn from(engine: &TutorialEngine) -> Self {
        let (completed, total) = engine.overall_progress();
        let tracks = engine
            .tracks()
            .iter()
            .map(|t| {
                let (completed, total) = engine.track_progress(&t.id);
                TrackStatus {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    completed,
                    total,
                    is_complete: engine.is_track_complete(&t.id),
                }
            })
            .collect();

        TutorialStatus {
            current_track: engine.state().current_track.clone(),
            current_exercise: engine.state().current_exercise.clone(),
            completed_exercises: completed,
            total_exercises: total,
            achievements_earned: engine.state().achievements.len(),
            total_time_minutes: engine.state().total_time_minutes,
            tracks,
        }
    }
}

// =============================================================================
// TutorialEnvironment: adapt exercises to the user's detected environment
// =============================================================================

/// Snapshot of the user's environment for contextual exercise adaptation.
///
/// Built from [`DetectedEnvironment`] plus lightweight DB/config checks.
/// Each field maps to a [`Requirement`] variant so `can_run_exercise` can
/// decide whether to run, simulate, or skip an exercise.
#[derive(Debug, Clone, Serialize)]
pub struct TutorialEnvironment {
    pub wezterm_running: bool,
    pub wezterm_version: Option<String>,
    pub pane_count: usize,
    pub agent_panes: Vec<AgentInfo>,
    pub wa_configured: bool,
    pub db_has_data: bool,
    pub shell_integration: bool,
}

/// Minimal agent info surfaced to the tutorial.
#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    pub agent_type: String,
    pub pane_id: u64,
}

impl TutorialEnvironment {
    /// Build a tutorial environment from a detected environment plus extra checks.
    ///
    /// `wa_configured` and `db_has_data` must be supplied by the caller because
    /// they depend on workspace layout / DB access that `DetectedEnvironment`
    /// doesn't cover.
    pub fn from_detected(
        env: &DetectedEnvironment,
        wa_configured: bool,
        db_has_data: bool,
    ) -> Self {
        Self {
            wezterm_running: env.wezterm.is_running,
            wezterm_version: env.wezterm.version.clone(),
            pane_count: env.agents.len()
                + env.remotes.iter().map(|r| r.pane_ids.len()).sum::<usize>(),
            agent_panes: env
                .agents
                .iter()
                .map(|a| AgentInfo {
                    agent_type: format!("{:?}", a.agent_type),
                    pane_id: a.pane_id,
                })
                .collect(),
            wa_configured,
            db_has_data,
            shell_integration: env.shell.osc_133_enabled,
        }
    }

    /// Check whether an exercise can run in this environment.
    pub fn can_run_exercise(&self, exercise: &Exercise) -> CanRun {
        for req in &exercise.requirements {
            let met = match req {
                Requirement::WeztermRunning => self.wezterm_running,
                Requirement::AgentPresent => !self.agent_panes.is_empty(),
                Requirement::WatcherRunning => {
                    // Approximation: if WezTerm is running and we have data, watcher is likely up
                    self.wezterm_running && self.db_has_data
                }
                Requirement::DbHasData => self.db_has_data,
                Requirement::WaConfigured => self.wa_configured,
            };

            if !met {
                let reason = match req {
                    Requirement::WeztermRunning => "Start WezTerm first",
                    Requirement::AgentPresent => "No agents detected",
                    Requirement::WatcherRunning => "Start the watcher first (wa watch)",
                    Requirement::DbHasData => "No captured data yet",
                    Requirement::WaConfigured => "Run wa setup first",
                };

                return if exercise.can_simulate {
                    CanRun::Simulation(reason)
                } else {
                    CanRun::No(reason)
                };
            }
        }

        CanRun::Yes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_state_machine_start_track() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::StartTrack("basics".into()))
            .unwrap();

        assert_eq!(engine.state().current_track, Some("basics".into()));
        assert_eq!(engine.state().current_exercise, Some("basics.1".into()));
    }

    #[test]
    fn test_state_machine_complete_exercise() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::StartTrack("basics".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
            .unwrap();

        assert!(engine.state().completed_exercises.contains("basics.1"));
        assert_eq!(engine.state().current_exercise, Some("basics.2".into()));
    }

    #[test]
    fn test_state_machine_reset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::StartTrack("basics".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
            .unwrap();
        engine.handle_event(TutorialEvent::Reset).unwrap();

        assert!(engine.state().completed_exercises.is_empty());
        assert!(engine.state().current_track.is_none());
    }

    #[test]
    fn test_persistence_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");

        {
            let mut engine = TutorialEngine::load_or_create_at(path.clone()).unwrap();
            engine
                .handle_event(TutorialEvent::StartTrack("basics".into()))
                .unwrap();
            engine
                .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
                .unwrap();
            engine.save().unwrap();
        }

        {
            let engine = TutorialEngine::load_or_create_at(path).unwrap();
            assert_eq!(engine.state().current_track, Some("basics".into()));
            assert!(engine.state().completed_exercises.contains("basics.1"));
        }
    }

    #[test]
    fn test_track_progress() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        assert_eq!(engine.track_progress("basics"), (0, 5));

        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
            .unwrap();
        assert_eq!(engine.track_progress("basics"), (1, 5));

        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.2".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.3".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.4".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.5".into()))
            .unwrap();
        assert_eq!(engine.track_progress("basics"), (5, 5));
        assert!(engine.is_track_complete("basics"));
    }

    #[test]
    fn test_achievement_unlock() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.3".into()))
            .unwrap();

        // Should have unlocked "first_watch" achievement
        assert!(
            engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "first_watch")
        );
    }

    #[test]
    fn test_start_unknown_track_does_not_modify_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        // Start a valid track first
        engine
            .handle_event(TutorialEvent::StartTrack("basics".into()))
            .unwrap();
        assert_eq!(engine.state().current_track, Some("basics".into()));

        // Try to start an unknown track - should not change state
        engine
            .handle_event(TutorialEvent::StartTrack("nonexistent".into()))
            .unwrap();

        // State should remain unchanged
        assert_eq!(engine.state().current_track, Some("basics".into()));
        assert_eq!(engine.state().current_exercise, Some("basics.1".into()));
    }

    #[test]
    fn test_heartbeat_handles_negative_elapsed_time() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        // Heartbeat should not panic or add time if elapsed is negative (clock skew)
        // We can't easily simulate clock skew, but we can verify heartbeat works normally
        let initial_time = engine.state().total_time_minutes;
        engine.handle_event(TutorialEvent::Heartbeat).unwrap();

        // Time should not decrease
        assert!(engine.state().total_time_minutes >= initial_time);
    }

    // -----------------------------------------------------------------------
    // Extended state machine tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_skip_exercise_advances_without_completing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::StartTrack("basics".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::SkipExercise("basics.1".into()))
            .unwrap();

        // Advances to next exercise but doesn't mark as completed
        assert_eq!(engine.state().current_exercise, Some("basics.2".into()));
        assert!(!engine.state().completed_exercises.contains("basics.1"));
    }

    #[test]
    fn test_complete_last_exercise_clears_current() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::StartTrack("basics".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.2".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.3".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.4".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.5".into()))
            .unwrap();

        // After completing last exercise, current_exercise becomes None
        assert!(engine.state().current_exercise.is_none());
        assert!(engine.is_track_complete("basics"));
    }

    #[test]
    fn test_duplicate_completion_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::StartTrack("basics".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
            .unwrap();
        // Complete same exercise again
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
            .unwrap();

        assert!(engine.state().completed_exercises.contains("basics.1"));
        assert_eq!(engine.state().completed_exercises.len(), 1);
    }

    #[test]
    fn test_complete_exercise_without_track() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        // Complete exercise without starting a track — should still record
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
            .unwrap();

        assert!(engine.state().completed_exercises.contains("basics.1"));
    }

    #[test]
    fn test_skip_last_exercise_clears_current() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::StartTrack("basics".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::SkipExercise("basics.1".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::SkipExercise("basics.2".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::SkipExercise("basics.3".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::SkipExercise("basics.4".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::SkipExercise("basics.5".into()))
            .unwrap();

        assert!(engine.state().current_exercise.is_none());
        // But track is NOT complete since nothing was completed
        assert!(!engine.is_track_complete("basics"));
    }

    // -----------------------------------------------------------------------
    // Achievement tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_first_event_achievement() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::CompleteExercise("events.3".into()))
            .unwrap();

        assert!(
            engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "first_event")
        );
    }

    #[test]
    fn test_track_completion_achievement() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        for id in &[
            "events.1", "events.2", "events.3", "events.4", "events.5", "events.6",
        ] {
            engine
                .handle_event(TutorialEvent::CompleteExercise((*id).into()))
                .unwrap();
        }

        assert!(
            engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "track_events_complete")
        );
    }

    #[test]
    fn test_robot_track_completion_achievement() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        for id in &[
            "robot.1", "robot.2", "robot.3", "robot.4", "robot.5", "robot.6",
        ] {
            engine
                .handle_event(TutorialEvent::CompleteExercise((*id).into()))
                .unwrap();
        }

        assert!(
            engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "track_robot_complete")
        );
    }

    #[test]
    fn test_duplicate_achievement_not_added() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::UnlockAchievement {
                id: "custom".into(),
                name: "Custom".into(),
                description: "Test".into(),
            })
            .unwrap();
        engine
            .handle_event(TutorialEvent::UnlockAchievement {
                id: "custom".into(),
                name: "Custom".into(),
                description: "Test".into(),
            })
            .unwrap();

        let count = engine
            .state()
            .achievements
            .iter()
            .filter(|a| a.id == "custom")
            .count();
        assert_eq!(count, 1);
    }

    // -----------------------------------------------------------------------
    // Progress and status tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_overall_progress() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        let (completed, total) = engine.overall_progress();
        assert_eq!(completed, 0);
        assert_eq!(total, 33); // 5 basics + 6 events + 7 workflows + 6 robot + 9 advanced

        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
            .unwrap();
        let (completed, _) = engine.overall_progress();
        assert_eq!(completed, 1);
    }

    #[test]
    fn test_unknown_track_progress() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        assert_eq!(engine.track_progress("nonexistent"), (0, 0));
        assert!(!engine.is_track_complete("nonexistent"));
    }

    #[test]
    fn test_current_exercise_returns_details() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        assert!(engine.current_exercise().is_none());

        engine
            .handle_event(TutorialEvent::StartTrack("basics".into()))
            .unwrap();

        let ex = engine.current_exercise().unwrap();
        assert_eq!(ex.id, "basics.1");
        assert_eq!(ex.title, "What is wa?");
    }

    #[test]
    fn test_tutorial_status_from_engine() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::StartTrack("basics".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
            .unwrap();

        let status = TutorialStatus::from(&engine);
        assert_eq!(status.current_track.as_deref(), Some("basics"));
        assert_eq!(status.completed_exercises, 1);
        assert_eq!(status.total_exercises, 33);
        assert_eq!(status.tracks.len(), 5);
        assert_eq!(status.tracks[0].completed, 1);
        assert_eq!(status.tracks[0].total, 5);
        assert!(!status.tracks[0].is_complete);
    }

    #[test]
    fn test_tutorial_status_json_serializable() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        let status = TutorialStatus::from(&engine);
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("completed_exercises"));
        assert!(json.contains("total_exercises"));
    }

    // -----------------------------------------------------------------------
    // Persistence edge case tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_corrupt_progress_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        std::fs::write(&path, "not valid json").unwrap();

        let result = TutorialEngine::load_or_create_at(path);
        assert!(result.is_err());
    }

    #[test]
    fn test_progress_file_created_on_save() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("subdir").join("learn.json");

        let engine = TutorialEngine::load_or_create_at(path.clone()).unwrap();
        assert!(!path.exists());

        engine.save().unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_get_track_returns_correct_track() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        let track = engine.get_track("events").unwrap();
        assert_eq!(track.name, "Events");
        assert_eq!(track.exercises.len(), 6);

        assert!(engine.get_track("nonexistent").is_none());
    }

    #[test]
    fn test_builtin_tracks_have_exercises() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        assert_eq!(engine.tracks().len(), 5);
        for track in engine.tracks() {
            assert!(!track.exercises.is_empty());
            assert!(!track.name.is_empty());
            assert!(!track.id.is_empty());
        }
    }

    #[test]
    fn test_exercise_verification_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        let basics = engine.get_track("basics").unwrap();
        // basics.1 is informational and has no verification
        assert!(basics.exercises[0].verification_command.is_none());
        assert!(basics.exercises[0].verification_pattern.is_none());
        // basics.2 and basics.3 have verification commands
        assert!(basics.exercises[1].verification_command.is_some());
        assert!(basics.exercises[2].verification_command.is_some());
        // basics.4 has no verification
        assert!(basics.exercises[3].verification_command.is_none());
    }

    #[test]
    fn test_reset_clears_achievements_too() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.3".into()))
            .unwrap();
        assert!(!engine.state().achievements.is_empty());

        engine.handle_event(TutorialEvent::Reset).unwrap();
        assert!(engine.state().achievements.is_empty());
        assert!(engine.state().completed_exercises.is_empty());
    }

    #[test]
    fn test_switch_tracks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::StartTrack("basics".into()))
            .unwrap();
        assert_eq!(engine.state().current_track.as_deref(), Some("basics"));

        engine
            .handle_event(TutorialEvent::StartTrack("events".into()))
            .unwrap();
        assert_eq!(engine.state().current_track.as_deref(), Some("events"));
        assert_eq!(engine.state().current_exercise.as_deref(), Some("events.1"));
    }

    #[test]
    fn test_resume_track_skips_completed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::StartTrack("basics".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
            .unwrap();

        // Switch to events, then back to basics
        engine
            .handle_event(TutorialEvent::StartTrack("events".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::StartTrack("basics".into()))
            .unwrap();

        // Should resume at basics.2 (first incomplete)
        assert_eq!(engine.state().current_exercise.as_deref(), Some("basics.2"));
    }

    // -----------------------------------------------------------------------
    // Achievement system tests (wa-ogc.9)
    // -----------------------------------------------------------------------

    #[test]
    fn test_builtin_achievements_count() {
        // Spec requires 10+ achievements
        assert!(BUILTIN_ACHIEVEMENTS.len() >= 10);
    }

    #[test]
    fn test_builtin_achievements_unique_ids() {
        let ids: HashSet<&str> = BUILTIN_ACHIEVEMENTS.iter().map(|d| d.id).collect();
        assert_eq!(ids.len(), BUILTIN_ACHIEVEMENTS.len());
    }

    #[test]
    fn test_achievement_definition_lookup() {
        let def = achievement_definition("first_watch").unwrap();
        assert_eq!(def.name, "First Watch");
        assert_eq!(def.rarity, Rarity::Common);
        assert!(!def.secret);

        assert!(achievement_definition("nonexistent").is_none());
    }

    #[test]
    fn test_secret_achievements_exist() {
        let secrets: Vec<_> = BUILTIN_ACHIEVEMENTS.iter().filter(|d| d.secret).collect();
        assert!(
            secrets.len() >= 2,
            "Expected at least 2 secret achievements"
        );
        assert!(secrets.iter().any(|d| d.id == "speed_runner"));
        assert!(secrets.iter().any(|d| d.id == "night_owl"));
    }

    #[test]
    fn test_rarity_tiers_present() {
        let rarities: HashSet<Rarity> = BUILTIN_ACHIEVEMENTS.iter().map(|d| d.rarity).collect();
        assert!(rarities.contains(&Rarity::Common));
        assert!(rarities.contains(&Rarity::Uncommon));
        assert!(rarities.contains(&Rarity::Rare));
        assert!(rarities.contains(&Rarity::Epic));
    }

    #[test]
    fn test_rarity_display() {
        assert_eq!(Rarity::Common.label(), "Common");
        assert_eq!(Rarity::Uncommon.label(), "Uncommon");
        assert_eq!(Rarity::Rare.label(), "Rare");
        assert_eq!(Rarity::Epic.label(), "Epic");
        assert_eq!(format!("{}", Rarity::Epic), "Epic");
    }

    #[test]
    fn test_first_step_achievement() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        // Any exercise completion should trigger first_step
        engine
            .handle_event(TutorialEvent::CompleteExercise("workflows.1".into()))
            .unwrap();

        assert!(
            engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "first_step")
        );
    }

    #[test]
    fn test_searcher_achievement() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::CompleteExercise("events.4".into()))
            .unwrap();

        assert!(
            engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "searcher")
        );
    }

    #[test]
    fn test_workflow_runner_achievement() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::CompleteExercise("workflows.2".into()))
            .unwrap();

        assert!(
            engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "workflow_runner")
        );
    }

    #[test]
    fn test_explorer_achievement() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        // Complete one exercise from each track
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
            .unwrap();
        assert!(
            !engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "explorer")
        );

        engine
            .handle_event(TutorialEvent::CompleteExercise("events.1".into()))
            .unwrap();
        assert!(
            !engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "explorer")
        );

        engine
            .handle_event(TutorialEvent::CompleteExercise("workflows.1".into()))
            .unwrap();
        assert!(
            !engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "explorer")
        );
        engine
            .handle_event(TutorialEvent::CompleteExercise("robot.1".into()))
            .unwrap();
        assert!(
            !engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "explorer")
        );
        engine
            .handle_event(TutorialEvent::CompleteExercise("advanced.1".into()))
            .unwrap();
        assert!(
            engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "explorer"),
            "Explorer should unlock after touching all tracks"
        );
    }

    #[test]
    fn test_completionist_and_wa_master_achievements() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        // Complete everything
        let all_exercises = [
            "basics.1",
            "basics.2",
            "basics.3",
            "basics.4",
            "basics.5",
            "events.1",
            "events.2",
            "events.3",
            "events.4",
            "events.5",
            "events.6",
            "workflows.1",
            "workflows.2",
            "workflows.3",
            "workflows.4",
            "workflows.5",
            "workflows.6",
            "workflows.7",
            "robot.1",
            "robot.2",
            "robot.3",
            "robot.4",
            "robot.5",
            "robot.6",
            "advanced.1",
            "advanced.2",
            "advanced.3",
            "advanced.4",
            "advanced.5",
            "advanced.6",
            "advanced.7",
            "advanced.8",
            "advanced.9",
        ];
        for ex in &all_exercises {
            engine
                .handle_event(TutorialEvent::CompleteExercise((*ex).into()))
                .unwrap();
        }

        assert!(
            engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "completionist")
        );
        assert!(
            engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "wa_master")
        );
    }

    #[test]
    fn test_speed_runner_achievement_with_low_time() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        // total_time_minutes starts at 0 (default), which is <= 3
        // Complete all basics exercises
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.2".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.3".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.4".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.5".into()))
            .unwrap();

        assert!(
            engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "speed_runner"),
            "Speed runner should unlock when basics complete with low time"
        );
    }

    #[test]
    fn test_achievement_collection_all_definitions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        let collection = engine.achievement_collection();
        assert_eq!(collection.len(), BUILTIN_ACHIEVEMENTS.len());

        // All should be locked initially
        for entry in &collection {
            assert!(entry.unlocked_at.is_none());
        }
    }

    #[test]
    fn test_achievement_collection_shows_unlocked() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.3".into()))
            .unwrap();

        let collection = engine.achievement_collection();
        let unlocked: Vec<_> = collection
            .iter()
            .filter(|e| e.unlocked_at.is_some())
            .collect();
        assert!(unlocked.len() >= 2); // first_step + first_watch at minimum
        assert!(unlocked.iter().any(|e| e.id == "first_step"));
        assert!(unlocked.iter().any(|e| e.id == "first_watch"));
    }

    #[test]
    fn test_achievement_collection_secret_flag() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        let collection = engine.achievement_collection();
        let secrets: Vec<_> = collection.iter().filter(|e| e.secret).collect();
        assert!(secrets.len() >= 2);
        // Locked secrets should be in collection but marked secret
        for s in &secrets {
            assert!(s.unlocked_at.is_none());
            assert!(s.secret);
        }
    }

    #[test]
    fn test_achievement_collection_json_serializable() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::CompleteExercise("events.3".into()))
            .unwrap();

        let collection = engine.achievement_collection();
        let json = serde_json::to_string_pretty(&collection).unwrap();
        assert!(json.contains("first_step"));
        assert!(json.contains("first_event"));
        assert!(json.contains("rarity"));
        assert!(json.contains("icon"));
    }

    #[test]
    fn test_format_achievement_unlock() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.3".into()))
            .unwrap();

        let achievement = engine
            .state()
            .achievements
            .iter()
            .find(|a| a.id == "first_watch")
            .unwrap();

        let display = TutorialEngine::format_achievement_unlock(achievement);
        assert!(display.contains("Achievement Unlocked!"));
        assert!(display.contains("First Watch"));
    }

    #[test]
    fn test_format_achievement_list_locked() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        let list = engine.format_achievement_list();
        assert!(list.contains("Achievements [0/"));
        // Secret achievements should show as ???
        assert!(list.contains("???"));
        // Non-secret locked should show name
        assert!(list.contains("First Watch"));
    }

    #[test]
    fn test_format_achievement_list_mixed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.3".into()))
            .unwrap();

        let list = engine.format_achievement_list();
        // Should show some unlocked
        assert!(!list.starts_with("Achievements [0/"));
        // Should still contain secret ???
        assert!(list.contains("???"));
    }

    #[test]
    fn test_achievement_definitions_have_icons() {
        for def in BUILTIN_ACHIEVEMENTS {
            assert!(
                def.icon != '\0',
                "Achievement {} should have a non-null icon",
                def.id
            );
        }
    }

    #[test]
    fn test_multiple_achievements_single_completion() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        // Completing basics.3 should unlock both first_step and first_watch
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.3".into()))
            .unwrap();

        let ids: Vec<&str> = engine
            .state()
            .achievements
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert!(ids.contains(&"first_step"));
        assert!(ids.contains(&"first_watch"));
    }

    #[test]
    fn test_achievement_persists_across_sessions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");

        // Session 1: earn an achievement
        {
            let mut engine = TutorialEngine::load_or_create_at(path.clone()).unwrap();
            engine
                .handle_event(TutorialEvent::CompleteExercise("events.3".into()))
                .unwrap();
            engine.save().unwrap();
        }

        // Session 2: check it's still there
        {
            let engine = TutorialEngine::load_or_create_at(path).unwrap();
            let collection = engine.achievement_collection();
            let first_event = collection.iter().find(|e| e.id == "first_event").unwrap();
            assert!(first_event.unlocked_at.is_some());
        }
    }

    #[test]
    fn test_rarity_serde_roundtrip() {
        let json = serde_json::to_string(&Rarity::Uncommon).unwrap();
        assert_eq!(json, r#""uncommon""#);
        let back: Rarity = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Rarity::Uncommon);
    }

    // -----------------------------------------------------------------------
    // TutorialEnvironment and CanRun tests (wa-ogc.2)
    // -----------------------------------------------------------------------

    use crate::environment::{
        DetectedAgent, DetectedEnvironment, RemoteHost, ShellInfo, SystemInfo, WeztermCapabilities,
        WeztermInfo,
    };
    use crate::patterns::AgentType;

    /// Build a minimal `DetectedEnvironment` for testing.
    fn make_env(
        wezterm_running: bool,
        osc_133: bool,
        agents: Vec<DetectedAgent>,
    ) -> DetectedEnvironment {
        DetectedEnvironment {
            wezterm: WeztermInfo {
                version: if wezterm_running {
                    Some("20240101-000000-abc".into())
                } else {
                    None
                },
                socket_path: None,
                is_running: wezterm_running,
                capabilities: WeztermCapabilities::default(),
            },
            shell: ShellInfo {
                shell_path: Some("/bin/bash".into()),
                shell_type: Some("bash".into()),
                version: None,
                config_file: None,
                osc_133_enabled: osc_133,
            },
            agents,
            remotes: vec![],
            system: SystemInfo {
                os: "linux".into(),
                arch: "x86_64".into(),
                cpu_count: 4,
                memory_mb: Some(8192),
                load_average: None,
                detected_at_epoch_ms: 0,
            },
            detected_at: Utc::now(),
        }
    }

    fn make_agent(pane_id: u64) -> DetectedAgent {
        DetectedAgent {
            agent_type: AgentType::ClaudeCode,
            pane_id,
            confidence: 0.95,
            indicators: vec!["claude-code".into()],
        }
    }

    #[test]
    fn test_tutorial_env_from_detected_basic() {
        let env = make_env(true, true, vec![make_agent(1)]);
        let tenv = TutorialEnvironment::from_detected(&env, true, true);

        assert!(tenv.wezterm_running);
        assert!(tenv.wezterm_version.is_some());
        assert_eq!(tenv.agent_panes.len(), 1);
        assert!(tenv.wa_configured);
        assert!(tenv.db_has_data);
        assert!(tenv.shell_integration);
    }

    #[test]
    fn test_tutorial_env_from_detected_nothing_running() {
        let env = make_env(false, false, vec![]);
        let tenv = TutorialEnvironment::from_detected(&env, false, false);

        assert!(!tenv.wezterm_running);
        assert!(tenv.wezterm_version.is_none());
        assert!(tenv.agent_panes.is_empty());
        assert!(!tenv.wa_configured);
        assert!(!tenv.db_has_data);
        assert!(!tenv.shell_integration);
    }

    #[test]
    fn test_can_run_no_requirements_returns_yes() {
        let env = make_env(false, false, vec![]);
        let tenv = TutorialEnvironment::from_detected(&env, false, false);

        let exercise = Exercise {
            id: "test.1".into(),
            title: "No reqs".into(),
            description: String::new(),
            instructions: vec![],
            verification_command: None,
            verification_pattern: None,
            requirements: vec![],
            can_simulate: false,
        };

        assert_eq!(tenv.can_run_exercise(&exercise), CanRun::Yes);
    }

    #[test]
    fn test_can_run_wezterm_requirement_met() {
        let env = make_env(true, false, vec![]);
        let tenv = TutorialEnvironment::from_detected(&env, false, false);

        let exercise = Exercise {
            id: "test.1".into(),
            title: "Needs WezTerm".into(),
            description: String::new(),
            instructions: vec![],
            verification_command: None,
            verification_pattern: None,
            requirements: vec![Requirement::WeztermRunning],
            can_simulate: false,
        };

        assert_eq!(tenv.can_run_exercise(&exercise), CanRun::Yes);
    }

    #[test]
    fn test_can_run_wezterm_requirement_not_met_no_simulate() {
        let env = make_env(false, false, vec![]);
        let tenv = TutorialEnvironment::from_detected(&env, false, false);

        let exercise = Exercise {
            id: "test.1".into(),
            title: "Needs WezTerm".into(),
            description: String::new(),
            instructions: vec![],
            verification_command: None,
            verification_pattern: None,
            requirements: vec![Requirement::WeztermRunning],
            can_simulate: false,
        };

        assert_eq!(
            tenv.can_run_exercise(&exercise),
            CanRun::No("Start WezTerm first")
        );
    }

    #[test]
    fn test_can_run_requirement_not_met_with_simulate() {
        let env = make_env(false, false, vec![]);
        let tenv = TutorialEnvironment::from_detected(&env, false, false);

        let exercise = Exercise {
            id: "test.1".into(),
            title: "Needs WezTerm".into(),
            description: String::new(),
            instructions: vec![],
            verification_command: None,
            verification_pattern: None,
            requirements: vec![Requirement::WeztermRunning],
            can_simulate: true,
        };

        assert_eq!(
            tenv.can_run_exercise(&exercise),
            CanRun::Simulation("Start WezTerm first")
        );
    }

    #[test]
    fn test_can_run_agent_present_requirement() {
        let env = make_env(true, false, vec![make_agent(1)]);
        let tenv = TutorialEnvironment::from_detected(&env, false, false);

        let exercise = Exercise {
            id: "test.1".into(),
            title: "Needs agent".into(),
            description: String::new(),
            instructions: vec![],
            verification_command: None,
            verification_pattern: None,
            requirements: vec![Requirement::AgentPresent],
            can_simulate: false,
        };

        assert_eq!(tenv.can_run_exercise(&exercise), CanRun::Yes);
    }

    #[test]
    fn test_can_run_agent_not_present() {
        let env = make_env(true, false, vec![]);
        let tenv = TutorialEnvironment::from_detected(&env, false, false);

        let exercise = Exercise {
            id: "test.1".into(),
            title: "Needs agent".into(),
            description: String::new(),
            instructions: vec![],
            verification_command: None,
            verification_pattern: None,
            requirements: vec![Requirement::AgentPresent],
            can_simulate: true,
        };

        assert_eq!(
            tenv.can_run_exercise(&exercise),
            CanRun::Simulation("No agents detected")
        );
    }

    #[test]
    fn test_can_run_db_has_data_requirement() {
        let env = make_env(false, false, vec![]);
        let tenv = TutorialEnvironment::from_detected(&env, false, true);

        let exercise = Exercise {
            id: "test.1".into(),
            title: "Needs data".into(),
            description: String::new(),
            instructions: vec![],
            verification_command: None,
            verification_pattern: None,
            requirements: vec![Requirement::DbHasData],
            can_simulate: false,
        };

        assert_eq!(tenv.can_run_exercise(&exercise), CanRun::Yes);
    }

    #[test]
    fn test_can_run_multiple_requirements_first_fails() {
        let env = make_env(false, false, vec![]);
        let tenv = TutorialEnvironment::from_detected(&env, true, true);

        let exercise = Exercise {
            id: "test.1".into(),
            title: "Needs wezterm + watcher".into(),
            description: String::new(),
            instructions: vec![],
            verification_command: None,
            verification_pattern: None,
            requirements: vec![Requirement::WeztermRunning, Requirement::WatcherRunning],
            can_simulate: false,
        };

        // First unsatisfied requirement should be reported
        assert_eq!(
            tenv.can_run_exercise(&exercise),
            CanRun::No("Start WezTerm first")
        );
    }

    #[test]
    fn test_can_run_wa_configured_requirement() {
        let env = make_env(false, false, vec![]);
        let tenv_yes = TutorialEnvironment::from_detected(&env, true, false);
        let tenv_no = TutorialEnvironment::from_detected(&env, false, false);

        let exercise = Exercise {
            id: "test.1".into(),
            title: "Needs config".into(),
            description: String::new(),
            instructions: vec![],
            verification_command: None,
            verification_pattern: None,
            requirements: vec![Requirement::WaConfigured],
            can_simulate: true,
        };

        assert_eq!(tenv_yes.can_run_exercise(&exercise), CanRun::Yes);
        assert_eq!(
            tenv_no.can_run_exercise(&exercise),
            CanRun::Simulation("Run wa setup first")
        );
    }

    #[test]
    fn test_pane_count_includes_remotes() {
        let mut env = make_env(true, false, vec![make_agent(1)]);
        env.remotes = vec![RemoteHost {
            hostname: "remote-host".into(),
            connection_type: crate::environment::ConnectionType::Ssh,
            pane_ids: vec![10, 11, 12],
        }];
        let tenv = TutorialEnvironment::from_detected(&env, false, false);

        // 1 agent + 3 remote panes = 4
        assert_eq!(tenv.pane_count, 4);
    }

    #[test]
    fn test_builtin_exercises_have_requirements_field() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        // basics.1 should have no requirements (can always run)
        let basics = engine.get_track("basics").unwrap();
        assert_eq!(basics.exercises.len(), 5);
        assert!(basics.exercises.iter().all(|e| e.can_simulate));
        assert!(basics.exercises[0].requirements.is_empty());
        assert!(basics.exercises[0].can_simulate);

        // basics.3 requires WezTerm
        assert!(
            basics.exercises[2]
                .requirements
                .contains(&Requirement::WeztermRunning)
        );
        assert!(basics.exercises[2].can_simulate);

        // events.3 requires data
        let events = engine.get_track("events").unwrap();
        assert_eq!(events.exercises.len(), 6);
        assert!(events.exercises.iter().all(|e| e.can_simulate));
        assert!(
            events.exercises[2]
                .requirements
                .contains(&Requirement::DbHasData)
        );

        // workflows.4 requires WaConfigured + WatcherRunning
        let workflows = engine.get_track("workflows").unwrap();
        assert_eq!(workflows.exercises.len(), 7);
        assert!(workflows.exercises.iter().all(|e| e.can_simulate));
        assert!(
            workflows.exercises[3]
                .requirements
                .contains(&Requirement::WaConfigured)
        );
        assert!(
            workflows.exercises[3]
                .requirements
                .contains(&Requirement::WatcherRunning)
        );

        // robot.5 requires DbHasData and all Robot exercises support simulation
        let robot = engine.get_track("robot").unwrap();
        assert_eq!(robot.exercises.len(), 6);
        assert!(robot.exercises.iter().all(|e| e.can_simulate));
        assert!(
            robot.exercises[4]
                .requirements
                .contains(&Requirement::DbHasData)
        );
        // robot.6 requires WaConfigured
        assert!(
            robot.exercises[5]
                .requirements
                .contains(&Requirement::WaConfigured)
        );
    }

    // -----------------------------------------------------------------------
    // Track 5 (Advanced) tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_advanced_track_exists_with_9_exercises() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        let track = engine.get_track("advanced").unwrap();
        assert_eq!(track.name, "Advanced");
        assert_eq!(track.exercises.len(), 9);
        assert_eq!(track.estimated_minutes, 20);
    }

    #[test]
    fn test_advanced_track_exercise_ids_sequential() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        let track = engine.get_track("advanced").unwrap();
        for (i, ex) in track.exercises.iter().enumerate() {
            assert_eq!(ex.id, format!("advanced.{}", i + 1));
        }
    }

    #[test]
    fn test_advanced_track_completion_achievement() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        for i in 1..=9 {
            engine
                .handle_event(TutorialEvent::CompleteExercise(
                    format!("advanced.{i}").into(),
                ))
                .unwrap();
        }

        assert!(
            engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "track_advanced_complete")
        );
    }

    #[test]
    fn test_advanced_track_all_exercises_simulatable() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        let track = engine.get_track("advanced").unwrap();
        for ex in &track.exercises {
            assert!(ex.can_simulate, "exercise {} must be simulatable", ex.id);
        }
    }

    #[test]
    fn test_advanced_track_requirements() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        let track = engine.get_track("advanced").unwrap();
        // advanced.1 (conceptual) has no requirements
        assert!(track.exercises[0].requirements.is_empty());
        // advanced.2 (create pattern) requires WaConfigured
        assert!(
            track.exercises[1]
                .requirements
                .contains(&Requirement::WaConfigured)
        );
        // advanced.5 (event correlation) requires DbHasData
        assert!(
            track.exercises[4]
                .requirements
                .contains(&Requirement::DbHasData)
        );
        // advanced.9 (track completion) has no requirements
        assert!(track.exercises[8].requirements.is_empty());
    }

    #[test]
    fn test_advanced_track_has_verification() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        let track = engine.get_track("advanced").unwrap();
        // advanced.2 (create pattern) has verification
        assert!(track.exercises[1].verification_command.is_some());
        assert!(track.exercises[1].verification_pattern.is_some());
        // advanced.1 (conceptual) has none
        assert!(track.exercises[0].verification_command.is_none());
    }

    #[test]
    fn test_advanced_partial_does_not_unlock_completion() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        // Complete only 8 of 9
        for i in 1..=8 {
            engine
                .handle_event(TutorialEvent::CompleteExercise(
                    format!("advanced.{i}").into(),
                ))
                .unwrap();
        }

        assert!(
            !engine
                .state()
                .achievements
                .iter()
                .any(|a| a.id == "track_advanced_complete")
        );
    }
}
