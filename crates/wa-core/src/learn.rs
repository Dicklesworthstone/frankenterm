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

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, instrument, warn};

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
                description: "What is wa? Start watching. View status.".into(),
                estimated_minutes: 5,
                exercises: vec![
                    Exercise {
                        id: "basics.1".into(),
                        title: "Check your environment".into(),
                        description: "Verify wa can connect to WezTerm".into(),
                        instructions: vec![
                            "Run: wa doctor".into(),
                            "Check that WezTerm is detected".into(),
                        ],
                        verification_command: Some("wa doctor --json".into()),
                        verification_pattern: Some("wezterm.*ok".into()),
                    },
                    Exercise {
                        id: "basics.2".into(),
                        title: "Start the watcher".into(),
                        description: "Launch the wa daemon to observe terminal activity".into(),
                        instructions: vec![
                            "Run: wa watch".into(),
                            "The watcher starts in the background".into(),
                        ],
                        verification_command: Some("wa status --json".into()),
                        verification_pattern: Some("running.*true".into()),
                    },
                    Exercise {
                        id: "basics.3".into(),
                        title: "View pane status".into(),
                        description: "See what panes wa is observing".into(),
                        instructions: vec!["Run: wa list".into(), "Run: wa status".into()],
                        verification_command: None,
                        verification_pattern: None,
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
                        title: "View detected events".into(),
                        description: "See what patterns wa has detected".into(),
                        instructions: vec![
                            "Run: wa robot events".into(),
                            "Look for pattern matches".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
                    },
                    Exercise {
                        id: "events.2".into(),
                        title: "Search pane output".into(),
                        description: "Use FTS5 to search captured output".into(),
                        instructions: vec![
                            "Run: wa search \"error\"".into(),
                            "Try different search terms".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
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
                        title: "List workflows".into(),
                        description: "See available workflow definitions".into(),
                        instructions: vec!["Run: wa workflow list".into()],
                        verification_command: None,
                        verification_pattern: None,
                    },
                    Exercise {
                        id: "workflows.2".into(),
                        title: "Run a workflow manually".into(),
                        description: "Trigger a workflow on demand".into(),
                        instructions: vec![
                            "Run: wa workflow run <name>".into(),
                            "Check the output".into(),
                        ],
                        verification_command: None,
                        verification_pattern: None,
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

        // First watch achievement
        if self.state.completed_exercises.contains("basics.2")
            && !self
                .state
                .achievements
                .iter()
                .any(|a| a.id == "first_watch")
        {
            to_add.push((
                "first_watch".into(),
                "First Watch".into(),
                "Started the wa watcher for the first time".into(),
            ));
        }

        // First event achievement
        if self.state.completed_exercises.contains("events.1")
            && !self
                .state
                .achievements
                .iter()
                .any(|a| a.id == "first_event")
        {
            to_add.push((
                "first_event".into(),
                "Event Spotter".into(),
                "Viewed your first detected event".into(),
            ));
        }

        // Track completion achievements
        for track in &self.tracks {
            let all_complete = track
                .exercises
                .iter()
                .all(|e| self.state.completed_exercises.contains(&e.id));
            let achievement_id = format!("track_{}_complete", track.id);

            if all_complete
                && !self
                    .state
                    .achievements
                    .iter()
                    .any(|a| a.id == achievement_id)
            {
                to_add.push((
                    achievement_id,
                    format!("{} Master", track.name),
                    format!("Completed all {} exercises", track.name),
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

        assert_eq!(engine.track_progress("basics"), (0, 3));

        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.1".into()))
            .unwrap();
        assert_eq!(engine.track_progress("basics"), (1, 3));

        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.2".into()))
            .unwrap();
        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.3".into()))
            .unwrap();
        assert_eq!(engine.track_progress("basics"), (3, 3));
        assert!(engine.is_track_complete("basics"));
    }

    #[test]
    fn test_achievement_unlock() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.2".into()))
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
            .handle_event(TutorialEvent::CompleteExercise("events.1".into()))
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

        for id in &["events.1", "events.2"] {
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
        assert_eq!(total, 7); // 3 basics + 2 events + 2 workflows

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
        assert_eq!(ex.title, "Check your environment");
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
        assert_eq!(status.total_exercises, 7);
        assert_eq!(status.tracks.len(), 3);
        assert_eq!(status.tracks[0].completed, 1);
        assert_eq!(status.tracks[0].total, 3);
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
        assert_eq!(track.exercises.len(), 2);

        assert!(engine.get_track("nonexistent").is_none());
    }

    #[test]
    fn test_builtin_tracks_have_exercises() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let engine = TutorialEngine::load_or_create_at(path).unwrap();

        assert_eq!(engine.tracks().len(), 3);
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
        // basics.1 has verification
        assert!(basics.exercises[0].verification_command.is_some());
        assert!(basics.exercises[0].verification_pattern.is_some());
        // basics.3 has no verification
        assert!(basics.exercises[2].verification_command.is_none());
    }

    #[test]
    fn test_reset_clears_achievements_too() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("learn.json");
        let mut engine = TutorialEngine::load_or_create_at(path).unwrap();

        engine
            .handle_event(TutorialEvent::CompleteExercise("basics.2".into()))
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
        assert_eq!(
            engine.state().current_exercise.as_deref(),
            Some("events.1")
        );
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
        assert_eq!(
            engine.state().current_exercise.as_deref(),
            Some("basics.2")
        );
    }
}
