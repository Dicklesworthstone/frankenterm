//! Host function audit trail for WASM extensions.
//!
//! Every host function call from a WASM extension is recorded in
//! a bounded in-memory ring buffer. Entries include the extension id,
//! function name, arguments summary, and result (ok/denied/error).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

/// Outcome of a host function call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditOutcome {
    /// Call succeeded.
    Ok,
    /// Call was denied by sandbox policy.
    Denied(String),
    /// Call failed with an error.
    Error(String),
}

/// Single audit log entry.
#[derive(Clone, Debug)]
pub struct AuditEntry {
    /// Monotonic timestamp relative to audit trail creation.
    pub elapsed: std::time::Duration,
    /// Extension identifier.
    pub extension_id: String,
    /// Host function name.
    pub function: String,
    /// Summary of arguments (truncated for large payloads).
    pub args_summary: String,
    /// Call outcome.
    pub outcome: AuditOutcome,
}

/// Thread-safe audit trail with a bounded capacity.
pub struct AuditTrail {
    entries: Mutex<VecDeque<AuditEntry>>,
    capacity: usize,
    epoch: Instant,
}

impl AuditTrail {
    /// Create a new audit trail with the given maximum entry count.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(capacity.min(16384))),
            capacity,
            epoch: Instant::now(),
        }
    }

    /// Record a host function call.
    pub fn record(
        &self,
        extension_id: &str,
        function: &str,
        args_summary: &str,
        outcome: AuditOutcome,
    ) {
        let entry = AuditEntry {
            elapsed: self.epoch.elapsed(),
            extension_id: extension_id.to_string(),
            function: function.to_string(),
            args_summary: truncate_string(args_summary, 256),
            outcome,
        };

        if let Ok(mut entries) = self.entries.lock() {
            if entries.len() >= self.capacity {
                entries.pop_front();
            }
            entries.push_back(entry);
        }
    }

    /// Retrieve recent audit entries (newest last).
    pub fn recent(&self, limit: usize) -> Vec<AuditEntry> {
        let entries = match self.entries.lock() {
            Ok(e) => e,
            Err(_) => return vec![],
        };
        let skip = entries.len().saturating_sub(limit);
        entries.iter().skip(skip).cloned().collect()
    }

    /// Count entries matching the given extension id.
    pub fn count_for_extension(&self, extension_id: &str) -> usize {
        match self.entries.lock() {
            Ok(entries) => entries
                .iter()
                .filter(|e| e.extension_id == extension_id)
                .count(),
            Err(_) => 0,
        }
    }

    /// Count denied calls.
    pub fn denied_count(&self) -> usize {
        match self.entries.lock() {
            Ok(entries) => entries
                .iter()
                .filter(|e| matches!(e.outcome, AuditOutcome::Denied(_)))
                .count(),
            Err(_) => 0,
        }
    }

    /// Total number of entries currently stored.
    pub fn len(&self) -> usize {
        self.entries.lock().map(|e| e.len()).unwrap_or(0)
    }

    /// Whether the audit trail is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn truncate_string(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let mut truncated = s[..max_len].to_string();
        truncated.push_str("...");
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_record_and_retrieve() {
        let trail = AuditTrail::new(100);
        trail.record(
            "ext-1",
            "ft_get_config",
            "\"color_scheme\"",
            AuditOutcome::Ok,
        );
        trail.record(
            "ext-2",
            "ft_get_pane_text",
            "0, 1000",
            AuditOutcome::Denied("no_pane_access".to_string()),
        );

        assert_eq!(trail.len(), 2);
        let recent = trail.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].extension_id, "ext-1");
        assert_eq!(recent[0].outcome, AuditOutcome::Ok);
        assert_eq!(recent[1].extension_id, "ext-2");
        assert!(
            matches!(&recent[1].outcome, AuditOutcome::Denied(reason) if reason == "no_pane_access")
        );
    }

    #[test]
    fn capacity_eviction() {
        let trail = AuditTrail::new(3);
        for i in 0..5 {
            trail.record(&format!("ext-{i}"), "fn", "", AuditOutcome::Ok);
        }
        assert_eq!(trail.len(), 3);
        let recent = trail.recent(10);
        // Should have ext-2, ext-3, ext-4 (oldest evicted)
        assert_eq!(recent[0].extension_id, "ext-2");
        assert_eq!(recent[2].extension_id, "ext-4");
    }

    #[test]
    fn count_for_extension() {
        let trail = AuditTrail::new(100);
        trail.record("ext-a", "fn1", "", AuditOutcome::Ok);
        trail.record("ext-b", "fn2", "", AuditOutcome::Ok);
        trail.record("ext-a", "fn3", "", AuditOutcome::Ok);

        assert_eq!(trail.count_for_extension("ext-a"), 2);
        assert_eq!(trail.count_for_extension("ext-b"), 1);
        assert_eq!(trail.count_for_extension("ext-c"), 0);
    }

    #[test]
    fn denied_count() {
        let trail = AuditTrail::new(100);
        trail.record("ext", "fn1", "", AuditOutcome::Ok);
        trail.record(
            "ext",
            "fn2",
            "",
            AuditOutcome::Denied("no_access".to_string()),
        );
        trail.record("ext", "fn3", "", AuditOutcome::Error("oops".to_string()));
        trail.record(
            "ext",
            "fn4",
            "",
            AuditOutcome::Denied("blocked".to_string()),
        );

        assert_eq!(trail.denied_count(), 2);
    }

    #[test]
    fn args_truncation() {
        let trail = AuditTrail::new(10);
        let long_args = "x".repeat(500);
        trail.record("ext", "fn", &long_args, AuditOutcome::Ok);

        let recent = trail.recent(1);
        assert!(recent[0].args_summary.len() < 300);
        assert!(recent[0].args_summary.ends_with("..."));
    }

    #[test]
    fn empty_trail() {
        let trail = AuditTrail::new(10);
        assert!(trail.is_empty());
        assert_eq!(trail.len(), 0);
        assert!(trail.recent(10).is_empty());
    }
}
