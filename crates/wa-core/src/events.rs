//! Event bus for detections and signals
//!
//! Provides bounded channels and fanout for system events.

use serde::{Deserialize, Serialize};

use crate::patterns::Detection;

/// Event types that flow through the system
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// New segment captured from a pane
    SegmentCaptured {
        pane_id: u64,
        seq: u64,
        content_len: usize,
    },

    /// Gap detected in capture stream
    GapDetected { pane_id: u64, reason: String },

    /// Pattern detected
    PatternDetected { pane_id: u64, detection: Detection },

    /// Pane discovered
    PaneDiscovered {
        pane_id: u64,
        domain: String,
        title: String,
    },

    /// Pane disappeared
    PaneDisappeared { pane_id: u64 },

    /// Workflow started
    WorkflowStarted {
        workflow_id: String,
        workflow_name: String,
        pane_id: u64,
    },

    /// Workflow step completed
    WorkflowStep {
        workflow_id: String,
        step_name: String,
        result: String,
    },

    /// Workflow completed
    WorkflowCompleted {
        workflow_id: String,
        success: bool,
        reason: Option<String>,
    },
}

/// Event bus for distributing events to subscribers
pub struct EventBus {
    /// Queue capacity for bounded channels
    capacity: usize,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1000)
    }
}

impl EventBus {
    /// Create a new event bus with specified queue capacity
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self { capacity }
    }

    /// Get the queue capacity
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Publish an event to all subscribers
    pub fn publish(&self, _event: Event) {
        // TODO: Implement event fanout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serializes() {
        let event = Event::SegmentCaptured {
            pane_id: 1,
            seq: 42,
            content_len: 100,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("segment_captured"));
    }

    #[test]
    fn bus_can_be_created() {
        let bus = EventBus::new(100);
        assert_eq!(bus.capacity(), 100);
    }
}
