use anyhow::Result;
use frankenterm_dynamic::Value;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// Opaque id for registered hooks.
pub type HookId = u64;

/// Opaque id for loaded extensions.
pub type ExtensionId = u64;

/// Structured config value produced by `ScriptingEngine::eval_config`.
pub type ConfigValue = Value;

/// Log level used by script actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Action emitted by scripting hooks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    SetConfig { key: String, value: Value },
    SendInput { pane_id: Option<u64>, text: String },
    Log { level: LogLevel, message: String },
    Custom { name: String, payload: Value },
}

type HookFn = dyn Fn(&str, &Value) -> Result<Vec<Action>> + Send + Sync + 'static;

/// Hook registration payload used by engines and dispatcher.
#[derive(Clone)]
pub struct HookHandler {
    pub priority: i32,
    pub filter: Option<String>,
    callback: Arc<HookFn>,
}

impl HookHandler {
    pub fn new<F>(priority: i32, filter: Option<String>, callback: F) -> Self
    where
        F: Fn(&str, &Value) -> Result<Vec<Action>> + Send + Sync + 'static,
    {
        Self {
            priority,
            filter,
            callback: Arc::new(callback),
        }
    }

    pub fn call(&self, event: &str, payload: &Value) -> Result<Vec<Action>> {
        (self.callback)(event, payload)
    }

    pub fn matches_event(&self, event: &str) -> bool {
        self.filter
            .as_deref()
            .is_none_or(|pattern| pattern == event)
    }
}

impl fmt::Debug for HookHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookHandler")
            .field("priority", &self.priority)
            .field("filter", &self.filter)
            .finish_non_exhaustive()
    }
}

/// Runtime feature surface for a scripting engine.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct EngineCapabilities {
    pub supports_async: bool,
    pub supports_filesystem: bool,
    pub supports_network: bool,
    pub sandboxed: bool,
    pub max_memory_bytes: Option<usize>,
    pub max_execution_time: Option<Duration>,
}

impl EngineCapabilities {
    /// Merge two capability sets conservatively.
    pub fn merge_with(self, other: Self) -> Self {
        Self {
            supports_async: self.supports_async || other.supports_async,
            supports_filesystem: self.supports_filesystem || other.supports_filesystem,
            supports_network: self.supports_network || other.supports_network,
            sandboxed: self.sandboxed && other.sandboxed,
            max_memory_bytes: min_opt(self.max_memory_bytes, other.max_memory_bytes),
            max_execution_time: min_opt(self.max_execution_time, other.max_execution_time),
        }
    }
}

/// Engine-agnostic extension manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionManifest {
    pub id: String,
    pub version: String,
    pub entrypoint: Option<String>,
    pub metadata: Value,
}

impl Default for ExtensionManifest {
    fn default() -> Self {
        Self {
            id: String::new(),
            version: String::new(),
            entrypoint: None,
            metadata: Value::Null,
        }
    }
}

fn min_opt<T: Ord>(lhs: Option<T>, rhs: Option<T>) -> Option<T> {
    match (lhs, rhs) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ── min_opt ──────────────────────────────────────────────

    #[test]
    fn min_opt_both_some_returns_smaller() {
        assert_eq!(min_opt(Some(3), Some(7)), Some(3));
        assert_eq!(min_opt(Some(10), Some(2)), Some(2));
        assert_eq!(min_opt(Some(5), Some(5)), Some(5));
    }

    #[test]
    fn min_opt_one_none_returns_some() {
        assert_eq!(min_opt(Some(42_usize), None), Some(42));
        assert_eq!(min_opt(None, Some(42_usize)), Some(42));
    }

    #[test]
    fn min_opt_both_none_returns_none() {
        assert_eq!(min_opt::<usize>(None, None), None);
    }

    // ── HookHandler ──────────────────────────────────────────

    #[test]
    fn matches_event_none_filter_matches_any_event() {
        let handler = HookHandler::new(0, None, |_, _| Ok(vec![]));
        assert!(handler.matches_event("pane-output"));
        assert!(handler.matches_event("resize"));
        assert!(handler.matches_event(""));
    }

    #[test]
    fn matches_event_exact_filter_matches_only_that_event() {
        let handler = HookHandler::new(0, Some("resize".to_string()), |_, _| Ok(vec![]));
        assert!(handler.matches_event("resize"));
        assert!(!handler.matches_event("pane-output"));
        assert!(!handler.matches_event(""));
        assert!(!handler.matches_event("resize-end"));
    }

    #[test]
    fn matches_event_empty_filter_matches_only_empty_event() {
        let handler = HookHandler::new(0, Some(String::new()), |_, _| Ok(vec![]));
        assert!(handler.matches_event(""));
        assert!(!handler.matches_event("resize"));
    }

    #[test]
    fn hook_handler_call_invokes_callback_with_correct_args() {
        let handler = HookHandler::new(0, None, |event, payload| {
            Ok(vec![Action::Custom {
                name: event.to_string(),
                payload: payload.clone(),
            }])
        });

        let actions = handler
            .call("test-event", &Value::String("hello".to_string()))
            .unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            Action::Custom {
                name: "test-event".to_string(),
                payload: Value::String("hello".to_string()),
            }
        );
    }

    #[test]
    fn hook_handler_call_propagates_errors() {
        let handler = HookHandler::new(0, None, |_, _| Err(anyhow::anyhow!("handler failed")));
        let result = handler.call("evt", &Value::Null);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("handler failed"));
    }

    #[test]
    fn hook_handler_priority_determines_sort_order() {
        let low = HookHandler::new(-10, None, |_, _| Ok(vec![]));
        let mid = HookHandler::new(0, None, |_, _| Ok(vec![]));
        let high = HookHandler::new(10, None, |_, _| Ok(vec![]));

        let mut handlers = vec![&high, &low, &mid];
        handlers.sort_by_key(|h| h.priority);
        assert_eq!(handlers[0].priority, -10);
        assert_eq!(handlers[1].priority, 0);
        assert_eq!(handlers[2].priority, 10);
    }

    #[test]
    fn hook_handler_is_clone() {
        let handler = HookHandler::new(5, Some("evt".to_string()), |_, _| Ok(vec![]));
        let cloned = handler.clone();
        assert_eq!(cloned.priority, 5);
        assert_eq!(cloned.filter, Some("evt".to_string()));
    }

    #[test]
    fn hook_handler_debug_output() {
        let handler = HookHandler::new(3, Some("resize".to_string()), |_, _| Ok(vec![]));
        let debug = format!("{handler:?}");
        assert!(debug.contains("priority: 3"));
        assert!(debug.contains("resize"));
    }

    // ── EngineCapabilities ───────────────────────────────────

    #[test]
    fn default_capabilities_all_false() {
        let caps = EngineCapabilities::default();
        assert!(!caps.supports_async);
        assert!(!caps.supports_filesystem);
        assert!(!caps.supports_network);
        assert!(!caps.sandboxed);
        assert_eq!(caps.max_memory_bytes, None);
        assert_eq!(caps.max_execution_time, None);
    }

    #[test]
    fn merge_with_or_semantics_for_feature_flags() {
        let a = EngineCapabilities {
            supports_async: true,
            supports_filesystem: false,
            ..Default::default()
        };
        let b = EngineCapabilities {
            supports_async: false,
            supports_filesystem: true,
            ..Default::default()
        };
        let merged = a.merge_with(b);
        assert!(merged.supports_async);
        assert!(merged.supports_filesystem);
    }

    #[test]
    fn merge_with_and_semantics_for_sandboxed() {
        let sandboxed = EngineCapabilities {
            sandboxed: true,
            ..Default::default()
        };
        let unsandboxed = EngineCapabilities {
            sandboxed: false,
            ..Default::default()
        };
        assert!(!sandboxed.clone().merge_with(unsandboxed).sandboxed);
        assert!(
            sandboxed
                .clone()
                .merge_with(EngineCapabilities {
                    sandboxed: true,
                    ..Default::default()
                })
                .sandboxed
        );
    }

    #[test]
    fn merge_with_takes_min_of_limits() {
        let a = EngineCapabilities {
            max_memory_bytes: Some(100),
            max_execution_time: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        let b = EngineCapabilities {
            max_memory_bytes: Some(50),
            max_execution_time: Some(Duration::from_secs(20)),
            ..Default::default()
        };
        let merged = a.merge_with(b);
        assert_eq!(merged.max_memory_bytes, Some(50));
        assert_eq!(merged.max_execution_time, Some(Duration::from_secs(10)));
    }

    #[test]
    fn merge_with_none_and_some_returns_some() {
        let a = EngineCapabilities {
            max_memory_bytes: None,
            max_execution_time: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        let b = EngineCapabilities {
            max_memory_bytes: Some(100),
            max_execution_time: None,
            ..Default::default()
        };
        let merged = a.merge_with(b);
        assert_eq!(merged.max_memory_bytes, Some(100));
        assert_eq!(merged.max_execution_time, Some(Duration::from_secs(5)));
    }

    #[test]
    fn merge_with_both_none_stays_none() {
        let a = EngineCapabilities::default();
        let b = EngineCapabilities::default();
        let merged = a.merge_with(b);
        assert_eq!(merged.max_memory_bytes, None);
        assert_eq!(merged.max_execution_time, None);
    }

    // ── ExtensionManifest ────────────────────────────────────

    #[test]
    fn extension_manifest_default_has_empty_fields() {
        let m = ExtensionManifest::default();
        assert!(m.id.is_empty());
        assert!(m.version.is_empty());
        assert!(m.entrypoint.is_none());
        assert_eq!(m.metadata, Value::Null);
    }

    #[test]
    fn extension_manifest_eq_and_clone() {
        let m = ExtensionManifest {
            id: "test".to_string(),
            version: "1.0.0".to_string(),
            entrypoint: Some("/path/to/ext.wasm".to_string()),
            metadata: Value::Bool(true),
        };
        let m2 = m.clone();
        assert_eq!(m, m2);
    }

    // ── Action ───────────────────────────────────────────────

    #[test]
    fn action_variants_construct_and_eq() {
        let set = Action::SetConfig {
            key: "font_size".to_string(),
            value: Value::U64(14),
        };
        let send = Action::SendInput {
            pane_id: Some(1),
            text: "ls\n".to_string(),
        };
        let log = Action::Log {
            level: LogLevel::Warn,
            message: "warning".to_string(),
        };
        let custom = Action::Custom {
            name: "custom".to_string(),
            payload: Value::Null,
        };

        assert_eq!(set.clone(), set);
        assert_ne!(set, send);
        assert_ne!(log.clone(), custom);
    }

    #[test]
    fn action_send_input_none_pane() {
        let action = Action::SendInput {
            pane_id: None,
            text: "broadcast".to_string(),
        };
        if let Action::SendInput { pane_id, text } = action {
            assert!(pane_id.is_none());
            assert_eq!(text, "broadcast");
        } else {
            panic!("expected SendInput");
        }
    }

    // ── LogLevel ─────────────────────────────────────────────

    #[test]
    fn log_level_copy_and_eq() {
        let level = LogLevel::Error;
        let copy = level;
        assert_eq!(level, copy);
        assert_ne!(LogLevel::Trace, LogLevel::Error);
    }

    // ── Property-based tests ─────────────────────────────────

    fn arb_capabilities() -> impl Strategy<Value = EngineCapabilities> {
        (
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            proptest::option::of(1_usize..1_000_000),
            proptest::option::of(1_u64..10_000),
        )
            .prop_map(|(async_, fs, net, sand, mem, time)| EngineCapabilities {
                supports_async: async_,
                supports_filesystem: fs,
                supports_network: net,
                sandboxed: sand,
                max_memory_bytes: mem,
                max_execution_time: time.map(Duration::from_millis),
            })
    }

    proptest! {
        #[test]
        fn merge_with_is_commutative_for_booleans(a in arb_capabilities(), b in arb_capabilities()) {
            let ab = a.clone().merge_with(b.clone());
            let ba = b.merge_with(a);
            prop_assert_eq!(ab.supports_async, ba.supports_async);
            prop_assert_eq!(ab.supports_filesystem, ba.supports_filesystem);
            prop_assert_eq!(ab.supports_network, ba.supports_network);
            prop_assert_eq!(ab.sandboxed, ba.sandboxed);
            prop_assert_eq!(ab.max_memory_bytes, ba.max_memory_bytes);
            prop_assert_eq!(ab.max_execution_time, ba.max_execution_time);
        }

        #[test]
        fn merge_with_default_is_identity_like(a in arb_capabilities()) {
            let merged = a.clone().merge_with(EngineCapabilities::default());
            prop_assert_eq!(merged.supports_async, a.supports_async);
            prop_assert_eq!(merged.supports_filesystem, a.supports_filesystem);
            prop_assert_eq!(merged.supports_network, a.supports_network);
            // sandboxed: a && false = false (unless a was false already)
            prop_assert_eq!(merged.sandboxed, a.sandboxed && false);
            // limits: Some merged with None = Some
            prop_assert_eq!(merged.max_memory_bytes, a.max_memory_bytes);
            prop_assert_eq!(merged.max_execution_time, a.max_execution_time);
        }

        #[test]
        fn min_opt_commutative(a in proptest::option::of(0_usize..10000), b in proptest::option::of(0_usize..10000)) {
            prop_assert_eq!(min_opt(a, b), min_opt(b, a));
        }

        #[test]
        fn matches_event_none_filter_always_true(event in "\\PC{1,50}") {
            let handler = HookHandler::new(0, None, |_, _| Ok(vec![]));
            prop_assert!(handler.matches_event(&event));
        }

        #[test]
        fn matches_event_with_filter_exact_only(filter in "\\PC{1,20}", event in "\\PC{1,20}") {
            let handler = HookHandler::new(0, Some(filter.clone()), |_, _| Ok(vec![]));
            prop_assert_eq!(handler.matches_event(&event), filter == event);
        }

        #[test]
        fn hook_priority_sorting_is_stable(
            priorities in prop::collection::vec(-100_i32..100, 1..50)
        ) {
            let handlers: Vec<HookHandler> = priorities
                .iter()
                .map(|&p| HookHandler::new(p, None, |_, _| Ok(vec![])))
                .collect();

            let mut sorted = handlers.clone();
            sorted.sort_by_key(|h| h.priority);

            for window in sorted.windows(2) {
                prop_assert!(window[0].priority <= window[1].priority);
            }
        }
    }
}
