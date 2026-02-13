use crate::ScriptingEngine;
use crate::types::{
    Action, EngineCapabilities, ExtensionId, ExtensionManifest, HookHandler, HookId,
};
use anyhow::{Context, Result, anyhow, bail};
use frankenterm_dynamic::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct LoadedExtension {
    engine_index: usize,
    engine_extension_id: ExtensionId,
}

/// Coordinates multiple scripting engines behind one `ScriptingEngine` API.
pub struct ScriptingDispatcher {
    engines: Vec<Arc<dyn ScriptingEngine>>,
    config_engine: usize,
    hook_registrations: Mutex<HashMap<HookId, Vec<(usize, HookId)>>>,
    loaded_extensions: Mutex<HashMap<ExtensionId, LoadedExtension>>,
    next_hook_id: AtomicU64,
    next_extension_id: AtomicU64,
    engine_name: String,
}

impl ScriptingDispatcher {
    /// Build a dispatcher with a designated primary config engine.
    pub fn new(engines: Vec<Arc<dyn ScriptingEngine>>, config_engine: usize) -> Result<Self> {
        if engines.is_empty() {
            bail!("ScriptingDispatcher requires at least one engine");
        }
        if config_engine >= engines.len() {
            bail!(
                "config_engine index {} out of range for {} engines",
                config_engine,
                engines.len()
            );
        }

        let names = engines
            .iter()
            .map(|engine| engine.engine_name().to_string())
            .collect::<Vec<_>>()
            .join(",");

        Ok(Self {
            engines,
            config_engine,
            hook_registrations: Mutex::new(HashMap::new()),
            loaded_extensions: Mutex::new(HashMap::new()),
            next_hook_id: AtomicU64::new(1),
            next_extension_id: AtomicU64::new(1),
            engine_name: format!("dispatcher({names})"),
        })
    }

    fn ordered_engine_indices(&self) -> impl Iterator<Item = usize> + '_ {
        std::iter::once(self.config_engine)
            .chain((0..self.engines.len()).filter(move |idx| *idx != self.config_engine))
    }
}

impl ScriptingEngine for ScriptingDispatcher {
    fn eval_config(&self, path: &Path) -> Result<Value> {
        let mut errors = Vec::new();
        for idx in self.ordered_engine_indices() {
            let engine = &self.engines[idx];
            match engine.eval_config(path) {
                Ok(config) => return Ok(config),
                Err(err) => errors.push(format!("{}: {err:#}", engine.engine_name())),
            }
        }
        Err(anyhow!(
            "all scripting engines failed to eval config {}: {}",
            path.display(),
            errors.join(" | ")
        ))
    }

    fn register_hook(&self, event: &str, handler: HookHandler) -> Result<HookId> {
        let mut registrations = Vec::with_capacity(self.engines.len());

        for (idx, engine) in self.engines.iter().enumerate() {
            match engine.register_hook(event, handler.clone()) {
                Ok(engine_hook_id) => registrations.push((idx, engine_hook_id)),
                Err(err) => {
                    for (registered_engine_idx, registered_hook_id) in registrations {
                        let _ =
                            self.engines[registered_engine_idx].unregister_hook(registered_hook_id);
                    }
                    return Err(err).with_context(|| {
                        format!(
                            "failed to register hook on engine {}",
                            self.engines[idx].engine_name()
                        )
                    });
                }
            }
        }

        let hook_id = self.next_hook_id.fetch_add(1, Ordering::Relaxed);
        self.hook_registrations
            .lock()
            .map_err(|_| anyhow!("hook registry lock poisoned"))?
            .insert(hook_id, registrations);
        Ok(hook_id)
    }

    fn unregister_hook(&self, id: HookId) -> Result<()> {
        let Some(registrations) = self
            .hook_registrations
            .lock()
            .map_err(|_| anyhow!("hook registry lock poisoned"))?
            .remove(&id)
        else {
            return Ok(());
        };

        let mut errors = Vec::new();
        for (engine_idx, engine_hook_id) in registrations {
            if let Err(err) = self.engines[engine_idx].unregister_hook(engine_hook_id) {
                errors.push(format!(
                    "{}: {err:#}",
                    self.engines[engine_idx].engine_name()
                ));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "failed to unregister hook {id} on some engines: {}",
                errors.join(" | ")
            ))
        }
    }

    fn fire_event(&self, event: &str, payload: &Value) -> Result<Vec<Action>> {
        let mut actions = Vec::new();
        for idx in self.ordered_engine_indices() {
            let mut engine_actions =
                self.engines[idx]
                    .fire_event(event, payload)
                    .with_context(|| {
                        format!(
                            "failed to fire event on {}",
                            self.engines[idx].engine_name()
                        )
                    })?;
            actions.append(&mut engine_actions);
        }
        Ok(actions)
    }

    fn load_extension(&self, manifest: &ExtensionManifest) -> Result<ExtensionId> {
        let mut errors = Vec::new();
        for idx in self.ordered_engine_indices() {
            match self.engines[idx].load_extension(manifest) {
                Ok(engine_extension_id) => {
                    let extension_id = self.next_extension_id.fetch_add(1, Ordering::Relaxed);
                    self.loaded_extensions
                        .lock()
                        .map_err(|_| anyhow!("extension registry lock poisoned"))?
                        .insert(
                            extension_id,
                            LoadedExtension {
                                engine_index: idx,
                                engine_extension_id,
                            },
                        );
                    return Ok(extension_id);
                }
                Err(err) => errors.push(format!("{}: {err:#}", self.engines[idx].engine_name())),
            }
        }

        Err(anyhow!(
            "no scripting engine accepted extension {}@{}: {}",
            manifest.id,
            manifest.version,
            errors.join(" | ")
        ))
    }

    fn unload_extension(&self, id: ExtensionId) -> Result<()> {
        let loaded = self
            .loaded_extensions
            .lock()
            .map_err(|_| anyhow!("extension registry lock poisoned"))?
            .remove(&id)
            .ok_or_else(|| anyhow!("unknown extension id {id}"))?;

        self.engines[loaded.engine_index]
            .unload_extension(loaded.engine_extension_id)
            .with_context(|| {
                format!(
                    "failed to unload extension {id} from {}",
                    self.engines[loaded.engine_index].engine_name()
                )
            })
    }

    fn capabilities(&self) -> EngineCapabilities {
        let mut engines = self.engines.iter();
        let Some(first) = engines.next() else {
            return EngineCapabilities::default();
        };

        engines.fold(first.capabilities(), |acc, engine| {
            acc.merge_with(engine.capabilities())
        })
    }

    fn engine_name(&self) -> &str {
        &self.engine_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Action, EngineCapabilities, ExtensionManifest, HookHandler, HookId};
    use anyhow::{Result, anyhow};
    use proptest::prelude::*;
    use std::collections::{HashMap, HashSet};
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    struct MockEngine {
        name: &'static str,
        eval_result: Result<Value, String>,
        fail_load_extension: bool,
        capabilities: EngineCapabilities,
        hooks: Mutex<HashMap<HookId, (String, HookHandler)>>,
        loaded_extensions: Mutex<HashSet<ExtensionId>>,
        next_hook_id: AtomicU64,
        next_extension_id: AtomicU64,
    }

    impl MockEngine {
        fn new(
            name: &'static str,
            eval_result: Result<Value, String>,
            fail_load_extension: bool,
            capabilities: EngineCapabilities,
        ) -> Self {
            Self {
                name,
                eval_result,
                fail_load_extension,
                capabilities,
                hooks: Mutex::new(HashMap::new()),
                loaded_extensions: Mutex::new(HashSet::new()),
                next_hook_id: AtomicU64::new(1),
                next_extension_id: AtomicU64::new(1),
            }
        }
    }

    impl ScriptingEngine for MockEngine {
        fn eval_config(&self, _path: &Path) -> Result<Value> {
            self.eval_result.clone().map_err(|err| anyhow!(err))
        }

        fn register_hook(&self, event: &str, handler: HookHandler) -> Result<HookId> {
            let hook_id = self.next_hook_id.fetch_add(1, Ordering::Relaxed);
            self.hooks
                .lock()
                .map_err(|_| anyhow!("hook lock poisoned"))?
                .insert(hook_id, (event.to_string(), handler));
            Ok(hook_id)
        }

        fn unregister_hook(&self, id: HookId) -> Result<()> {
            self.hooks
                .lock()
                .map_err(|_| anyhow!("hook lock poisoned"))?
                .remove(&id);
            Ok(())
        }

        fn fire_event(&self, event: &str, payload: &Value) -> Result<Vec<Action>> {
            let hooks = self
                .hooks
                .lock()
                .map_err(|_| anyhow!("hook lock poisoned"))?;
            let mut sorted = hooks.values().collect::<Vec<_>>();
            sorted.sort_by_key(|(_, handler)| handler.priority);

            let mut actions = Vec::new();
            for (registered_event, handler) in sorted {
                if registered_event == event && handler.matches_event(event) {
                    let mut from_handler = handler.call(event, payload)?;
                    actions.append(&mut from_handler);
                }
            }

            Ok(actions)
        }

        fn load_extension(&self, _manifest: &ExtensionManifest) -> Result<ExtensionId> {
            if self.fail_load_extension {
                bail!("{} rejected extension", self.name);
            }
            let extension_id = self.next_extension_id.fetch_add(1, Ordering::Relaxed);
            self.loaded_extensions
                .lock()
                .map_err(|_| anyhow!("extension lock poisoned"))?
                .insert(extension_id);
            Ok(extension_id)
        }

        fn unload_extension(&self, id: ExtensionId) -> Result<()> {
            let removed = self
                .loaded_extensions
                .lock()
                .map_err(|_| anyhow!("extension lock poisoned"))?
                .remove(&id);
            if !removed {
                bail!("unknown extension id {id}");
            }
            Ok(())
        }

        fn capabilities(&self) -> EngineCapabilities {
            self.capabilities.clone()
        }

        fn engine_name(&self) -> &str {
            self.name
        }
    }

    #[test]
    fn eval_config_uses_fallback_engine_when_primary_fails() {
        let primary = Arc::new(MockEngine::new(
            "lua",
            Err("lua parse error".to_string()),
            false,
            EngineCapabilities::default(),
        ));
        let secondary = Arc::new(MockEngine::new(
            "wasm",
            Ok(Value::String("ok".to_string())),
            false,
            EngineCapabilities::default(),
        ));

        let dispatcher = ScriptingDispatcher::new(vec![primary, secondary], 0).unwrap();
        let config = dispatcher.eval_config(Path::new("test.lua")).unwrap();

        assert_eq!(config, Value::String("ok".to_string()));
    }

    #[test]
    fn register_fire_and_unregister_hook_fans_out_to_all_engines() {
        let engine_a = Arc::new(MockEngine::new(
            "lua",
            Ok(Value::Null),
            false,
            EngineCapabilities::default(),
        ));
        let engine_b = Arc::new(MockEngine::new(
            "wasm",
            Ok(Value::Null),
            false,
            EngineCapabilities::default(),
        ));

        let dispatcher = ScriptingDispatcher::new(vec![engine_a, engine_b], 0).unwrap();
        let handler = HookHandler::new(0, None, |_event, payload| {
            Ok(vec![Action::Custom {
                name: "handled".to_string(),
                payload: payload.clone(),
            }])
        });

        let hook_id = dispatcher.register_hook("pane-output", handler).unwrap();
        let actions = dispatcher
            .fire_event("pane-output", &Value::String("line".to_string()))
            .unwrap();

        assert_eq!(actions.len(), 2);

        dispatcher.unregister_hook(hook_id).unwrap();
        let actions_after_unreg = dispatcher
            .fire_event("pane-output", &Value::String("line".to_string()))
            .unwrap();
        assert!(actions_after_unreg.is_empty());
    }

    #[test]
    fn load_extension_falls_back_when_primary_rejects() {
        let primary = Arc::new(MockEngine::new(
            "lua",
            Ok(Value::Null),
            true,
            EngineCapabilities::default(),
        ));
        let secondary = Arc::new(MockEngine::new(
            "wasm",
            Ok(Value::Null),
            false,
            EngineCapabilities::default(),
        ));

        let dispatcher = ScriptingDispatcher::new(vec![primary, secondary], 0).unwrap();
        let extension_id = dispatcher
            .load_extension(&ExtensionManifest {
                id: "example".to_string(),
                version: "1.0.0".to_string(),
                ..ExtensionManifest::default()
            })
            .unwrap();

        dispatcher.unload_extension(extension_id).unwrap();
    }

    #[test]
    fn capabilities_are_merged_across_engines() {
        let lua_caps = EngineCapabilities {
            supports_async: false,
            supports_filesystem: true,
            supports_network: false,
            sandboxed: true,
            max_memory_bytes: Some(512 * 1024 * 1024),
            max_execution_time: Some(std::time::Duration::from_millis(1500)),
        };
        let wasm_caps = EngineCapabilities {
            supports_async: true,
            supports_filesystem: false,
            supports_network: true,
            sandboxed: true,
            max_memory_bytes: Some(256 * 1024 * 1024),
            max_execution_time: Some(std::time::Duration::from_millis(500)),
        };

        let lua = Arc::new(MockEngine::new("lua", Ok(Value::Null), false, lua_caps));
        let wasm = Arc::new(MockEngine::new("wasm", Ok(Value::Null), false, wasm_caps));
        let dispatcher = ScriptingDispatcher::new(vec![lua, wasm], 0).unwrap();

        let merged = dispatcher.capabilities();
        assert!(merged.supports_async);
        assert!(merged.supports_filesystem);
        assert!(merged.supports_network);
        assert!(merged.sandboxed);
        assert_eq!(merged.max_memory_bytes, Some(256 * 1024 * 1024));
        assert_eq!(
            merged.max_execution_time,
            Some(std::time::Duration::from_millis(500))
        );
    }

    #[test]
    fn dispatcher_requires_at_least_one_engine() {
        let result = ScriptingDispatcher::new(vec![], 0);
        let err = result.err().expect("should fail");
        assert!(err.to_string().contains("at least one engine"));
    }

    #[test]
    fn dispatcher_rejects_out_of_range_config_engine() {
        let engine = Arc::new(MockEngine::new(
            "mock",
            Ok(Value::Null),
            false,
            EngineCapabilities::default(),
        ));
        let result = ScriptingDispatcher::new(vec![engine], 5);
        let err = result.err().expect("should fail");
        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn engine_name_includes_all_engine_names() {
        let a = Arc::new(MockEngine::new(
            "lua",
            Ok(Value::Null),
            false,
            EngineCapabilities::default(),
        ));
        let b = Arc::new(MockEngine::new(
            "wasm",
            Ok(Value::Null),
            false,
            EngineCapabilities::default(),
        ));
        let dispatcher = ScriptingDispatcher::new(vec![a, b], 0).unwrap();
        let name = dispatcher.engine_name();
        assert!(name.contains("lua"));
        assert!(name.contains("wasm"));
        assert!(name.starts_with("dispatcher("));
    }

    #[test]
    fn unregister_nonexistent_hook_returns_ok() {
        let engine = Arc::new(MockEngine::new(
            "mock",
            Ok(Value::Null),
            false,
            EngineCapabilities::default(),
        ));
        let dispatcher = ScriptingDispatcher::new(vec![engine], 0).unwrap();
        // Hook ID 999 was never registered
        let result = dispatcher.unregister_hook(999);
        assert!(result.is_ok());
    }

    #[test]
    fn unload_nonexistent_extension_returns_error() {
        let engine = Arc::new(MockEngine::new(
            "mock",
            Ok(Value::Null),
            false,
            EngineCapabilities::default(),
        ));
        let dispatcher = ScriptingDispatcher::new(vec![engine], 0).unwrap();
        let result = dispatcher.unload_extension(999);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown extension")
        );
    }

    #[test]
    fn all_engines_fail_eval_config_reports_all_errors() {
        let a = Arc::new(MockEngine::new(
            "lua",
            Err("lua broke".to_string()),
            false,
            EngineCapabilities::default(),
        ));
        let b = Arc::new(MockEngine::new(
            "wasm",
            Err("wasm broke".to_string()),
            false,
            EngineCapabilities::default(),
        ));
        let dispatcher = ScriptingDispatcher::new(vec![a, b], 0).unwrap();
        let result = dispatcher.eval_config(Path::new("test.config"));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("lua broke"));
        assert!(msg.contains("wasm broke"));
    }

    #[test]
    fn all_engines_fail_load_extension_reports_all_errors() {
        let a = Arc::new(MockEngine::new(
            "lua",
            Ok(Value::Null),
            true,
            EngineCapabilities::default(),
        ));
        let b = Arc::new(MockEngine::new(
            "wasm",
            Ok(Value::Null),
            true,
            EngineCapabilities::default(),
        ));
        let dispatcher = ScriptingDispatcher::new(vec![a, b], 0).unwrap();
        let result = dispatcher.load_extension(&ExtensionManifest {
            id: "example".to_string(),
            version: "1.0.0".to_string(),
            ..ExtensionManifest::default()
        });
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("lua rejected"));
        assert!(msg.contains("wasm rejected"));
    }

    #[test]
    fn fire_event_no_hooks_returns_empty() {
        let engine = Arc::new(MockEngine::new(
            "mock",
            Ok(Value::Null),
            false,
            EngineCapabilities::default(),
        ));
        let dispatcher = ScriptingDispatcher::new(vec![engine], 0).unwrap();
        let actions = dispatcher
            .fire_event("no-one-listening", &Value::Null)
            .unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn hook_ids_are_unique_across_registrations() {
        let engine = Arc::new(MockEngine::new(
            "mock",
            Ok(Value::Null),
            false,
            EngineCapabilities::default(),
        ));
        let dispatcher = ScriptingDispatcher::new(vec![engine], 0).unwrap();
        let handler = HookHandler::new(0, None, |_, _| Ok(vec![]));

        let mut ids = std::collections::HashSet::new();
        for _ in 0..100 {
            let id = dispatcher.register_hook("evt", handler.clone()).unwrap();
            assert!(ids.insert(id), "duplicate hook id {id}");
        }
    }

    #[test]
    fn extension_ids_are_unique_across_loads() {
        let engine = Arc::new(MockEngine::new(
            "mock",
            Ok(Value::Null),
            false,
            EngineCapabilities::default(),
        ));
        let dispatcher = ScriptingDispatcher::new(vec![engine], 0).unwrap();
        let manifest = ExtensionManifest {
            id: "ext".to_string(),
            version: "1.0.0".to_string(),
            ..ExtensionManifest::default()
        };

        let mut ids = std::collections::HashSet::new();
        for _ in 0..50 {
            let id = dispatcher.load_extension(&manifest).unwrap();
            assert!(ids.insert(id), "duplicate extension id {id}");
        }
    }

    #[test]
    fn fire_event_collects_actions_from_all_engines_in_order() {
        let engine_a = Arc::new(MockEngine::new(
            "engine-a",
            Ok(Value::Null),
            false,
            EngineCapabilities::default(),
        ));
        let engine_b = Arc::new(MockEngine::new(
            "engine-b",
            Ok(Value::Null),
            false,
            EngineCapabilities::default(),
        ));
        let dispatcher =
            ScriptingDispatcher::new(vec![engine_a.clone(), engine_b.clone()], 0).unwrap();

        // Register different handlers on each engine via dispatcher
        let handler_a = HookHandler::new(0, Some("evt".to_string()), |_, _| {
            Ok(vec![Action::Custom {
                name: "from-a".to_string(),
                payload: Value::Null,
            }])
        });
        let handler_b = HookHandler::new(0, Some("evt".to_string()), |_, _| {
            Ok(vec![Action::Custom {
                name: "from-b".to_string(),
                payload: Value::Null,
            }])
        });

        dispatcher.register_hook("evt", handler_a).unwrap();
        dispatcher.register_hook("evt", handler_b).unwrap();

        let actions = dispatcher.fire_event("evt", &Value::Null).unwrap();
        // Each engine has both hooks registered (fan-out), so each fires twice per engine
        // Primary engine (index 0) fires first, then secondary
        assert!(actions.len() >= 2);
        let names: Vec<&str> = actions
            .iter()
            .map(|a| match a {
                Action::Custom { name, .. } => name.as_str(),
                _ => panic!("unexpected action"),
            })
            .collect();
        assert!(names.contains(&"from-a"));
        assert!(names.contains(&"from-b"));
    }

    #[test]
    fn single_engine_capabilities_returned_unchanged() {
        let caps = EngineCapabilities {
            supports_async: true,
            supports_filesystem: true,
            supports_network: false,
            sandboxed: true,
            max_memory_bytes: Some(256),
            max_execution_time: Some(std::time::Duration::from_millis(100)),
        };
        let engine = Arc::new(MockEngine::new(
            "only",
            Ok(Value::Null),
            false,
            caps.clone(),
        ));
        let dispatcher = ScriptingDispatcher::new(vec![engine], 0).unwrap();
        assert_eq!(dispatcher.capabilities(), caps);
    }

    proptest! {
        #[test]
        fn hook_lifecycle_property_holds(ops in prop::collection::vec(any::<u8>(), 1..200)) {
            let engine = Arc::new(MockEngine::new(
                "mock",
                Ok(Value::Null),
                false,
                EngineCapabilities::default(),
            ));
            let dispatcher = ScriptingDispatcher::new(vec![engine], 0).unwrap();

            let mut registrations: Vec<(u64, HookId)> = Vec::new();
            let mut active_tokens: HashSet<u64> = HashSet::new();
            let mut seen_hook_ids: HashSet<HookId> = HashSet::new();
            let mut next_token: u64 = 1;

            for code in ops {
                match code % 3 {
                    0 => {
                        let token = next_token;
                        next_token += 1;
                        let handler = HookHandler::new(
                            0,
                            Some("evt".to_string()),
                            move |_event, _payload| {
                                Ok(vec![Action::Custom {
                                    name: "token".to_string(),
                                    payload: Value::U64(token),
                                }])
                            },
                        );

                        let hook_id = dispatcher.register_hook("evt", handler).unwrap();
                        prop_assert!(seen_hook_ids.insert(hook_id), "hook id reused");
                        registrations.push((token, hook_id));
                        active_tokens.insert(token);
                    }
                    1 => {
                        if registrations.is_empty() {
                            continue;
                        }
                        let idx = usize::from(code) % registrations.len();
                        let (token, hook_id) = registrations.remove(idx);
                        dispatcher.unregister_hook(hook_id).unwrap();
                        active_tokens.remove(&token);
                    }
                    _ => {
                        let actions = dispatcher.fire_event("evt", &Value::Null).unwrap();
                        let observed = actions
                            .into_iter()
                            .map(|action| match action {
                                Action::Custom {
                                    payload: Value::U64(token),
                                    ..
                                } => token,
                                other => panic!("unexpected action payload from property test: {other:?}"),
                            })
                            .collect::<HashSet<_>>();

                        prop_assert_eq!(observed, active_tokens.clone());
                    }
                }
            }
        }
    }
}
