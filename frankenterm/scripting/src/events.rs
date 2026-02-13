//! Event system for extension hooks.
//!
//! Provides a typed event bus where native handlers, WASM extensions,
//! and Lua extensions can all register hooks. Dispatch order:
//! native (Rust) → WASM → Lua, with priority ordering within each tier.

use crate::types::Action;
use anyhow::Result;
use frankenterm_dynamic::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Well-known event types that FrankenTerm fires.
pub mod event_types {
    pub const CONFIG_RELOAD: &str = "config.reload";
    pub const PANE_FOCUS: &str = "pane.focus";
    pub const PANE_CREATED: &str = "pane.created";
    pub const PANE_CLOSED: &str = "pane.closed";
    pub const PANE_OUTPUT: &str = "pane.output";
    pub const PANE_TITLE_CHANGED: &str = "pane.title_changed";
    pub const TAB_CREATED: &str = "tab.created";
    pub const TAB_CLOSED: &str = "tab.closed";
    pub const WINDOW_FOCUS: &str = "window.focus";
    pub const KEY_PRESSED: &str = "key.pressed";
    pub const SESSION_SAVE: &str = "session.save";
    pub const SESSION_RESTORE: &str = "session.restore";
}

/// Dispatch tier (determines execution order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DispatchTier {
    /// Rust native handlers — fastest, run first.
    Native = 0,
    /// WASM extension handlers — sandboxed, ~100μs overhead.
    Wasm = 1,
    /// Lua extension handlers — GIL, ~1ms overhead.
    Lua = 2,
}

/// Opaque handle returned by hook registration.
pub type EventHookId = u64;

type NativeHandlerFn = dyn Fn(&str, &Value) -> Result<Vec<Action>> + Send + Sync + 'static;

/// A registered event hook.
struct RegisteredHook {
    id: EventHookId,
    event_pattern: String,
    tier: DispatchTier,
    priority: i32,
    extension_id: Option<String>,
    handler: Arc<NativeHandlerFn>,
}

/// Central event bus for the extension system.
///
/// Hooks are dispatched in tier order (Native → Wasm → Lua),
/// and within each tier, in priority order (lower number = higher priority).
pub struct EventBus {
    hooks: Mutex<Vec<RegisteredHook>>,
    next_id: AtomicU64,
    /// Stats: number of events fired per event type.
    fire_counts: Mutex<HashMap<String, u64>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self {
            hooks: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            fire_counts: Mutex::new(HashMap::new()),
        }
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a hook for the given event pattern.
    ///
    /// The pattern can be an exact event name or `"*"` for a catch-all.
    /// Returns a handle that can be used to unregister the hook.
    pub fn register<F>(
        &self,
        event_pattern: &str,
        tier: DispatchTier,
        priority: i32,
        extension_id: Option<&str>,
        handler: F,
    ) -> EventHookId
    where
        F: Fn(&str, &Value) -> Result<Vec<Action>> + Send + Sync + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let hook = RegisteredHook {
            id,
            event_pattern: event_pattern.to_string(),
            tier,
            priority,
            extension_id: extension_id.map(String::from),
            handler: Arc::new(handler),
        };

        if let Ok(mut hooks) = self.hooks.lock() {
            hooks.push(hook);
        }

        id
    }

    /// Unregister a hook by its handle.
    pub fn unregister(&self, id: EventHookId) -> bool {
        if let Ok(mut hooks) = self.hooks.lock() {
            let len_before = hooks.len();
            hooks.retain(|h| h.id != id);
            return hooks.len() < len_before;
        }
        false
    }

    /// Unregister all hooks for a given extension.
    pub fn unregister_extension(&self, extension_id: &str) -> usize {
        if let Ok(mut hooks) = self.hooks.lock() {
            let len_before = hooks.len();
            hooks.retain(|h| h.extension_id.as_deref() != Some(extension_id));
            return len_before - hooks.len();
        }
        0
    }

    /// Fire an event and collect all resulting actions.
    ///
    /// Hooks are executed in tier order, then priority order within each tier.
    pub fn fire(&self, event: &str, payload: &Value) -> Result<Vec<Action>> {
        // Update fire count
        if let Ok(mut counts) = self.fire_counts.lock() {
            *counts.entry(event.to_string()).or_default() += 1;
        }

        // Collect matching hooks (sorted by tier then priority)
        let matching: Vec<(DispatchTier, i32, Arc<NativeHandlerFn>)> = {
            let hooks = self
                .hooks
                .lock()
                .map_err(|_| anyhow::anyhow!("lock poisoned"))?;
            let mut matched: Vec<_> = hooks
                .iter()
                .filter(|h| matches_event(&h.event_pattern, event))
                .map(|h| (h.tier, h.priority, Arc::clone(&h.handler)))
                .collect();
            matched.sort_by_key(|(tier, priority, _)| (*tier, *priority));
            matched
        };

        let mut actions = Vec::new();
        for (_, _, handler) in &matching {
            let mut hook_actions = handler(event, payload)?;
            actions.append(&mut hook_actions);
        }

        Ok(actions)
    }

    /// Number of currently registered hooks.
    pub fn hook_count(&self) -> usize {
        self.hooks.lock().map(|h| h.len()).unwrap_or(0)
    }

    /// Number of hooks registered for a specific event.
    pub fn hooks_for_event(&self, event: &str) -> usize {
        self.hooks
            .lock()
            .map(|hooks| {
                hooks
                    .iter()
                    .filter(|h| matches_event(&h.event_pattern, event))
                    .count()
            })
            .unwrap_or(0)
    }

    /// Get the number of times each event has been fired.
    pub fn fire_counts(&self) -> HashMap<String, u64> {
        self.fire_counts
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default()
    }

    /// List all registered hook IDs for an extension.
    pub fn hooks_for_extension(&self, extension_id: &str) -> Vec<EventHookId> {
        self.hooks
            .lock()
            .map(|hooks| {
                hooks
                    .iter()
                    .filter(|h| h.extension_id.as_deref() == Some(extension_id))
                    .map(|h| h.id)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Check if an event pattern matches a concrete event name.
fn matches_event(pattern: &str, event: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        return event.starts_with(prefix) && event[prefix.len()..].starts_with('.');
    }
    pattern == event
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_fire() {
        let bus = EventBus::new();
        bus.register(
            event_types::PANE_FOCUS,
            DispatchTier::Native,
            0,
            None,
            |_event, _payload| {
                Ok(vec![Action::Log {
                    level: crate::types::LogLevel::Info,
                    message: "focused".to_string(),
                }])
            },
        );

        let actions = bus.fire(event_types::PANE_FOCUS, &Value::Null).unwrap();
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn unregister_removes_hook() {
        let bus = EventBus::new();
        let id = bus.register(
            event_types::PANE_FOCUS,
            DispatchTier::Native,
            0,
            None,
            |_, _| Ok(vec![]),
        );

        assert_eq!(bus.hook_count(), 1);
        assert!(bus.unregister(id));
        assert_eq!(bus.hook_count(), 0);
    }

    #[test]
    fn unregister_extension_removes_all() {
        let bus = EventBus::new();
        bus.register("a", DispatchTier::Wasm, 0, Some("ext-1"), |_, _| Ok(vec![]));
        bus.register("b", DispatchTier::Wasm, 0, Some("ext-1"), |_, _| Ok(vec![]));
        bus.register("c", DispatchTier::Wasm, 0, Some("ext-2"), |_, _| Ok(vec![]));

        assert_eq!(bus.hook_count(), 3);
        let removed = bus.unregister_extension("ext-1");
        assert_eq!(removed, 2);
        assert_eq!(bus.hook_count(), 1);
    }

    #[test]
    fn dispatch_order_by_tier() {
        let bus = EventBus::new();
        let order = Arc::new(Mutex::new(Vec::new()));

        let o = Arc::clone(&order);
        bus.register("evt", DispatchTier::Lua, 0, None, move |_, _| {
            o.lock().unwrap().push("lua");
            Ok(vec![])
        });

        let o = Arc::clone(&order);
        bus.register("evt", DispatchTier::Native, 0, None, move |_, _| {
            o.lock().unwrap().push("native");
            Ok(vec![])
        });

        let o = Arc::clone(&order);
        bus.register("evt", DispatchTier::Wasm, 0, None, move |_, _| {
            o.lock().unwrap().push("wasm");
            Ok(vec![])
        });

        bus.fire("evt", &Value::Null).unwrap();

        let result = order.lock().unwrap();
        assert_eq!(*result, vec!["native", "wasm", "lua"]);
    }

    #[test]
    fn dispatch_order_by_priority_within_tier() {
        let bus = EventBus::new();
        let order = Arc::new(Mutex::new(Vec::new()));

        let o = Arc::clone(&order);
        bus.register("evt", DispatchTier::Native, 10, None, move |_, _| {
            o.lock().unwrap().push("low");
            Ok(vec![])
        });

        let o = Arc::clone(&order);
        bus.register("evt", DispatchTier::Native, 0, None, move |_, _| {
            o.lock().unwrap().push("high");
            Ok(vec![])
        });

        let o = Arc::clone(&order);
        bus.register("evt", DispatchTier::Native, 5, None, move |_, _| {
            o.lock().unwrap().push("mid");
            Ok(vec![])
        });

        bus.fire("evt", &Value::Null).unwrap();

        let result = order.lock().unwrap();
        assert_eq!(*result, vec!["high", "mid", "low"]);
    }

    #[test]
    fn wildcard_pattern_matches_all() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicU64::new(0));

        let c = Arc::clone(&count);
        bus.register("*", DispatchTier::Native, 0, None, move |_, _| {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(vec![])
        });

        bus.fire("pane.focus", &Value::Null).unwrap();
        bus.fire("config.reload", &Value::Null).unwrap();
        bus.fire("custom.event", &Value::Null).unwrap();

        assert_eq!(count.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn prefix_wildcard_pattern() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicU64::new(0));

        let c = Arc::clone(&count);
        bus.register("pane.*", DispatchTier::Native, 0, None, move |_, _| {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(vec![])
        });

        bus.fire("pane.focus", &Value::Null).unwrap();
        bus.fire("pane.closed", &Value::Null).unwrap();
        bus.fire("tab.created", &Value::Null).unwrap(); // should NOT match

        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn non_matching_events_not_dispatched() {
        let bus = EventBus::new();
        bus.register(
            event_types::PANE_FOCUS,
            DispatchTier::Native,
            0,
            None,
            |_, _| {
                Ok(vec![Action::Log {
                    level: crate::types::LogLevel::Info,
                    message: "fired".to_string(),
                }])
            },
        );

        let actions = bus.fire(event_types::TAB_CREATED, &Value::Null).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn fire_counts_tracked() {
        let bus = EventBus::new();
        bus.fire("pane.focus", &Value::Null).unwrap();
        bus.fire("pane.focus", &Value::Null).unwrap();
        bus.fire("config.reload", &Value::Null).unwrap();

        let counts = bus.fire_counts();
        assert_eq!(counts.get("pane.focus"), Some(&2));
        assert_eq!(counts.get("config.reload"), Some(&1));
    }

    #[test]
    fn hooks_for_extension() {
        let bus = EventBus::new();
        bus.register("a", DispatchTier::Wasm, 0, Some("my-ext"), |_, _| {
            Ok(vec![])
        });
        bus.register("b", DispatchTier::Wasm, 0, Some("my-ext"), |_, _| {
            Ok(vec![])
        });
        bus.register("c", DispatchTier::Wasm, 0, Some("other"), |_, _| Ok(vec![]));

        assert_eq!(bus.hooks_for_extension("my-ext").len(), 2);
        assert_eq!(bus.hooks_for_extension("other").len(), 1);
        assert_eq!(bus.hooks_for_extension("none").len(), 0);
    }

    #[test]
    fn matches_event_exact() {
        assert!(matches_event("pane.focus", "pane.focus"));
        assert!(!matches_event("pane.focus", "pane.closed"));
    }

    #[test]
    fn matches_event_wildcard() {
        assert!(matches_event("*", "anything"));
        assert!(matches_event("*", ""));
    }

    #[test]
    fn matches_event_prefix_wildcard() {
        assert!(matches_event("pane.*", "pane.focus"));
        assert!(matches_event("pane.*", "pane.closed"));
        assert!(!matches_event("pane.*", "tab.created"));
        assert!(!matches_event("pane.*", "pane"));
    }
}
