use crate::ScriptingEngine;
use crate::types::{
    Action, EngineCapabilities, ExtensionId, ExtensionManifest, HookHandler, HookId,
};
use anyhow::{Context, Result, anyhow, bail};
use config::Config;
use config::lua::{emit_sync_callback, make_lua_context};
use frankenterm_dynamic::{ToDynamic, Value};
use luahelper::{dynamic_to_lua_value, lua_value_to_dynamic};
use mlua::{FromLua, Lua};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

struct LuaEngineState {
    lua: Option<Lua>,
    hooks: HashMap<HookId, (String, HookHandler)>,
    extensions: HashMap<ExtensionId, ExtensionManifest>,
    next_hook_id: HookId,
    next_extension_id: ExtensionId,
}

impl Default for LuaEngineState {
    fn default() -> Self {
        Self {
            lua: None,
            hooks: HashMap::new(),
            extensions: HashMap::new(),
            next_hook_id: 1,
            next_extension_id: 1,
        }
    }
}

/// `ScriptingEngine` adapter for the existing Lua runtime.
pub struct LuaEngine {
    state: Mutex<LuaEngineState>,
}

impl Default for LuaEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaEngine {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(LuaEngineState::default()),
        }
    }

    pub fn with_lua_context(lua: Lua) -> Self {
        let state = LuaEngineState {
            lua: Some(lua),
            ..LuaEngineState::default()
        };
        Self {
            state: Mutex::new(state),
        }
    }

    fn state_guard(&self) -> MutexGuard<'_, LuaEngineState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                self.state.clear_poison();
                poisoned.into_inner()
            }
        }
    }
}

impl ScriptingEngine for LuaEngine {
    fn eval_config(&self, path: &Path) -> Result<Value> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("failed to read lua config {}", path.display()))?;

        let lua = make_lua_context(path)
            .with_context(|| format!("make_lua_context({})", path.display()))?;
        // Synchronous, CPU-bound config eval: use the sync `eval()`, never
        // `promise::spawn::block_on(eval_async())`. The latter panics if this is
        // ever reached on the GUI main-thread spawn queue (the same class as the
        // config.rs main-thread regression) and, on asupersync, `block_on` does
        // not drive I/O for a directly-polled future. Sync eval avoids the async
        // runtime entirely, which is what a synchronous config load wants.
        let config_value: mlua::Value = lua
            .load(source.trim_start_matches('\u{FEFF}'))
            .set_name(path.to_string_lossy())
            .eval()
            .with_context(|| format!("executing lua config {}", path.display()))?;

        let cfg = Config::from_lua(config_value, &lua).with_context(|| {
            format!(
                "converting lua value returned by {} to Config",
                path.display()
            )
        })?;
        cfg.check_consistency()
            .context("Config::check_consistency")?;
        let cfg = cfg.compute_extra_defaults(Some(path));

        let mut state = self.state_guard();
        state.lua = Some(lua);

        Ok(cfg.to_dynamic())
    }

    fn register_hook(&self, event: &str, handler: HookHandler) -> Result<HookId> {
        let mut state = self.state_guard();
        let hook_id =
            crate::try_next_unique_id_value(&mut state.next_hook_id).ok_or_else(|| {
                anyhow!("Lua hook id space exhausted; refusing to reuse a hook identity")
            })?;
        state.hooks.insert(hook_id, (event.to_string(), handler));
        Ok(hook_id)
    }

    fn unregister_hook(&self, id: HookId) -> Result<()> {
        self.state_guard().hooks.remove(&id);
        Ok(())
    }

    fn fire_event(&self, event: &str, payload: &Value) -> Result<Vec<Action>> {
        let mut handlers = {
            let state = self.state_guard();
            state
                .hooks
                .values()
                .filter(|(registered_event, handler)| {
                    registered_event == event && handler.matches_event(event)
                })
                .map(|(_, handler)| handler.clone())
                .collect::<Vec<_>>()
        };
        handlers.sort_by_key(|handler| handler.priority);

        let mut actions = Vec::new();
        // Run the Rust hook handlers with the state lock released. These are the
        // re-entrancy-prone callbacks (a handler may call back into the engine),
        // so they must not run while we hold `state`.
        for handler in handlers {
            let mut from_handler = handler.call(event, payload)?;
            actions.append(&mut from_handler);
        }

        // Emit the Lua-side `wezterm.on` handlers. `mlua::Lua` (0.9) is not
        // `Clone`, and under the `send` feature it is `Send` but not `Sync`, so
        // it cannot be cloned out of the lock or wrapped in `Arc`/`Rc` to drop
        // the guard. We instead borrow it under a fresh, short-lived guard. This
        // is safe against the non-reentrant `Mutex`: the callbacks execute inside
        // the Lua VM and hold no handle back into `LuaEngine`, so they cannot
        // re-enter `state_guard()`.
        {
            let state = self.state_guard();
            if let Some(lua) = state.lua.as_ref() {
                let lua_payload = dynamic_to_lua_value(lua, payload.clone())
                    .context("converting fire_event payload to lua value")?;
                let result = emit_sync_callback(lua, (event.to_string(), (lua_payload,)))
                    .with_context(|| format!("emit_sync_callback({event})"))?;
                if !matches!(result, mlua::Value::Nil) {
                    let dyn_result = lua_value_to_dynamic(result)
                        .context("converting lua callback result to dynamic value")?;
                    actions.push(Action::Custom {
                        name: format!("lua:{event}"),
                        payload: dyn_result,
                    });
                }
            }
        }

        Ok(actions)
    }

    fn load_extension(&self, manifest: &ExtensionManifest) -> Result<ExtensionId> {
        let entrypoint = manifest
            .entrypoint
            .as_deref()
            .ok_or_else(|| anyhow!("Lua extension {} missing entrypoint", manifest.id))?;
        if self.state_guard().next_extension_id == u64::MAX {
            bail!("Lua extension id space exhausted; refusing to reuse an extension identity");
        }
        let path = PathBuf::from(entrypoint);
        if !path.exists() {
            bail!(
                "Lua extension entrypoint does not exist: {}",
                path.display()
            );
        }

        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read extension script {}", path.display()))?;

        let mut state = self.state_guard();
        let extension_id = crate::try_next_unique_id_value(&mut state.next_extension_id)
            .ok_or_else(|| {
                anyhow!("Lua extension id space exhausted; refusing to reuse an extension identity")
            })?;
        if state.lua.is_none() {
            state.lua =
                Some(make_lua_context(&path).with_context(|| {
                    format!("make_lua_context for extension {}", path.display())
                })?);
        }

        let lua = state
            .lua
            .as_ref()
            .ok_or_else(|| anyhow!("lua state unexpectedly missing after initialization"))?;
        // Synchronous extension load: sync `exec()`, not block_on(exec_async())
        // (see eval_config above for the rationale).
        lua.load(&source)
            .set_name(path.to_string_lossy())
            .exec()
            .with_context(|| format!("executing lua extension {}", path.display()))?;

        state.extensions.insert(extension_id, manifest.clone());
        Ok(extension_id)
    }

    fn unload_extension(&self, id: ExtensionId) -> Result<()> {
        let removed = self.state_guard().extensions.remove(&id);
        if removed.is_none() {
            bail!("unknown extension id {id}");
        }
        Ok(())
    }

    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            supports_async: true,
            supports_filesystem: true,
            supports_network: true,
            sandboxed: false,
            max_memory_bytes: None,
            max_execution_time: None,
        }
    }

    fn engine_name(&self) -> &str {
        "lua-5.4"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScriptingEngine;
    use proptest::prelude::*;
    use std::ops::RangeInclusive;
    use tempfile::tempdir;

    fn dynamic_strategy() -> impl Strategy<Value = Value> {
        let uint_range: RangeInclusive<u64> = 0..=i64::MAX as u64;
        let leaf = prop_oneof![
            any::<bool>().prop_map(Value::Bool),
            ".*".prop_map(Value::String),
            any::<i64>().prop_map(Value::I64),
            uint_range.prop_map(Value::U64),
        ];

        leaf.prop_recursive(3, 64, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 1..4)
                    .prop_map(|vals| Value::Array(vals.into_iter().collect())),
                prop::collection::btree_map("[a-z_]{1,8}", inner, 1..4).prop_map(|entries| {
                    Value::Object(
                        entries
                            .into_iter()
                            .map(|(k, v)| (Value::String(k), v))
                            .collect(),
                    )
                }),
            ]
        })
    }

    fn normalize_lua_roundtrip(value: Value) -> Value {
        match value {
            Value::U64(v) if v <= i64::MAX as u64 => Value::I64(v as i64),
            Value::Array(array) => {
                Value::Array(array.into_iter().map(normalize_lua_roundtrip).collect())
            }
            Value::Object(object) => Value::Object(
                object
                    .into_iter()
                    .map(|(k, v)| (normalize_lua_roundtrip(k), normalize_lua_roundtrip(v)))
                    .collect(),
            ),
            other => other,
        }
    }

    #[test]
    fn capabilities_match_lua_runtime() {
        let engine = LuaEngine::new();
        let caps = engine.capabilities();
        assert!(caps.supports_async);
        assert!(caps.supports_filesystem);
        assert!(caps.supports_network);
        assert!(!caps.sandboxed);
    }

    #[test]
    fn exhausted_lua_hook_ids_fail_before_registration() {
        let engine = LuaEngine::new();
        engine.state_guard().next_hook_id = u64::MAX;
        let handler = HookHandler::new(0, None, |_event, _payload| Ok(Vec::new()));

        let error = engine
            .register_hook("evt", handler)
            .expect_err("exhausted Lua hook identities must reject registration");

        assert!(error.to_string().contains("hook id space exhausted"));
        assert!(engine.state_guard().hooks.is_empty());
    }

    #[test]
    fn exhausted_lua_extension_ids_fail_before_entrypoint_io() {
        let engine = LuaEngine::new();
        engine.state_guard().next_extension_id = u64::MAX;
        let manifest = ExtensionManifest {
            id: "must-not-load".to_string(),
            version: "1.0.0".to_string(),
            entrypoint: Some("/path/that/must/not/be/read.lua".to_string()),
            ..ExtensionManifest::default()
        };

        let error = engine
            .load_extension(&manifest)
            .expect_err("exhausted Lua extension identities must reject before I/O");

        assert!(error.to_string().contains("extension id space exhausted"));
        assert!(engine.state_guard().extensions.is_empty());
    }

    #[test]
    fn register_and_fire_rust_hook() {
        let engine = LuaEngine::new();
        let hook_id = engine
            .register_hook(
                "evt",
                HookHandler::new(0, Some("evt".to_string()), |_event, _payload| {
                    Ok(vec![Action::Custom {
                        name: "ok".to_string(),
                        payload: Value::Bool(true),
                    }])
                }),
            )
            .unwrap();

        let actions = engine.fire_event("evt", &Value::Null).unwrap();
        assert_eq!(actions.len(), 1);
        engine.unregister_hook(hook_id).unwrap();
    }

    #[test]
    fn state_recovers_after_poisoned_hook_lifecycle() {
        let engine = LuaEngine::new();

        let poison_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = engine.state.lock().unwrap();
            panic!("poison Lua engine state");
        }));
        assert!(poison_result.is_err());
        assert!(engine.state.is_poisoned());

        let hook_id = engine
            .register_hook(
                "evt",
                HookHandler::new(0, Some("evt".to_string()), |_event, _payload| {
                    Ok(vec![Action::Custom {
                        name: "recovered".to_string(),
                        payload: Value::Bool(true),
                    }])
                }),
            )
            .unwrap();
        assert!(!engine.state.is_poisoned());

        let actions = engine.fire_event("evt", &Value::Null).unwrap();
        assert_eq!(actions.len(), 1);

        engine.unregister_hook(hook_id).unwrap();
        let actions_after_unreg = engine.fire_event("evt", &Value::Null).unwrap();
        assert!(actions_after_unreg.is_empty());
    }

    #[test]
    fn fire_event_drops_state_lock_before_calling_handlers() {
        let engine = std::sync::Arc::new(LuaEngine::new());
        let engine_for_handler = std::sync::Arc::clone(&engine);

        engine
            .register_hook(
                "evt",
                HookHandler::new(0, Some("evt".to_string()), move |_event, _payload| {
                    let _guard = engine_for_handler
                        .state
                        .try_lock()
                        .expect("fire_event must not hold engine state while invoking handlers");
                    Ok(vec![Action::Custom {
                        name: "unlocked".to_string(),
                        payload: Value::Null,
                    }])
                }),
            )
            .unwrap();

        let actions = engine.fire_event("evt", &Value::Null).unwrap();
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn eval_config_loads_fixture_config() {
        let engine = LuaEngine::new();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/frankenterm-core/tests/fixtures/setup/wezterm_present.lua");
        let dynamic_cfg = engine.eval_config(&fixture).unwrap();
        assert!(matches!(dynamic_cfg, Value::Object(_)));
    }

    #[test]
    fn fire_event_surfaces_lua_callback_result() {
        let lua = make_lua_context(Path::new("testing")).unwrap();
        let callback: mlua::Function = lua
            .load(
                r#"
                return function(payload)
                  return { event = "seen", payload = payload }
                end
            "#,
            )
            .eval()
            .unwrap();
        config::lua::register_event(&lua, ("script-event".to_string(), callback)).unwrap();

        let engine = LuaEngine::with_lua_context(lua);
        let actions = engine
            .fire_event("script-event", &Value::String("hello".to_string()))
            .unwrap();

        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::Custom { name, .. } => assert_eq!(name, "lua:script-event"),
            other => panic!("unexpected action {other:?}"),
        }
    }

    #[test]
    fn load_extension_executes_lua_entrypoint() {
        let tempdir = tempdir().unwrap();
        let script_path = tempdir.path().join("extension.lua");
        std::fs::write(
            &script_path,
            r#"
local wezterm = require 'wezterm'
wezterm.on('extension-event', function() end)
"#,
        )
        .unwrap();

        let engine = LuaEngine::new();
        let extension_id = engine
            .load_extension(&ExtensionManifest {
                id: "lua-ext".to_string(),
                version: "0.1.0".to_string(),
                entrypoint: Some(script_path.to_string_lossy().into_owned()),
                metadata: Value::Null,
            })
            .unwrap();

        assert_eq!(extension_id, 1);
        engine.unload_extension(extension_id).unwrap();
    }

    #[test]
    fn state_recovers_after_poisoned_extension_lifecycle() {
        let tempdir = tempdir().unwrap();
        let script_path = tempdir.path().join("extension.lua");
        std::fs::write(&script_path, "local recovered = true").unwrap();

        let engine = LuaEngine::new();
        let poison_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = engine.state.lock().unwrap();
            panic!("poison Lua engine state");
        }));
        assert!(poison_result.is_err());
        assert!(engine.state.is_poisoned());

        let extension_id = engine
            .load_extension(&ExtensionManifest {
                id: "lua-ext".to_string(),
                version: "0.1.0".to_string(),
                entrypoint: Some(script_path.to_string_lossy().into_owned()),
                metadata: Value::Null,
            })
            .unwrap();
        assert!(!engine.state.is_poisoned());
        assert_eq!(extension_id, 1);

        engine.unload_extension(extension_id).unwrap();
    }

    proptest! {
        #[test]
        fn lua_dynamic_roundtrip(value in dynamic_strategy()) {
            let lua = make_lua_context(Path::new("testing")).unwrap();
            let lua_value = dynamic_to_lua_value(&lua, value.clone()).unwrap();
            let roundtrip = lua_value_to_dynamic(lua_value).unwrap();
            prop_assert_eq!(
                normalize_lua_roundtrip(roundtrip),
                normalize_lua_roundtrip(value)
            );
        }
    }
}
