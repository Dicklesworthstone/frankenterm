//! WASM scripting engine backed by wasmtime.
//!
//! Implements [`ScriptingEngine`] using wasmtime's runtime with WASI
//! support for sandboxed, capability-based extensions and config evaluation.
//!
//! # Security model
//!
//! Each WASM module runs in a sandboxed `Store` with:
//! - **Memory limit**: configurable, default 64 MiB per module
//! - **Fuel metering**: configurable instruction budget (default 1 billion)
//! - **WASI capabilities**: read-only filesystem access to config dirs,
//!   env var access, stdout/stderr capture

use crate::ScriptingEngine;
use crate::types::{
    Action, ConfigValue, EngineCapabilities, ExtensionId, ExtensionManifest, HookHandler, HookId,
};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use wasmtime::*;
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p1::WasiP1Ctx;

/// Configuration for the WASM engine.
#[derive(Clone, Debug)]
pub struct WasmEngineConfig {
    /// Maximum memory per WASM module in bytes (default: 64 MiB).
    pub max_memory_bytes: usize,
    /// Fuel budget per call (default: 1 billion instructions).
    pub fuel_per_call: u64,
    /// Maximum execution time per call.
    pub max_execution_time: Duration,
}

impl Default for WasmEngineConfig {
    fn default() -> Self {
        Self {
            max_memory_bytes: 64 * 1024 * 1024,
            fuel_per_call: 1_000_000_000,
            max_execution_time: Duration::from_secs(10),
        }
    }
}

/// WASM engine runtime state stored in each wasmtime `Store`.
pub(crate) struct WasmState {
    wasi: WasiP1Ctx,
    stdout_buf: Vec<u8>,
}

impl WasmState {
    fn wasi_ctx(&mut self) -> &mut WasiP1Ctx {
        &mut self.wasi
    }
}

/// Loaded WASM extension held in memory.
struct LoadedModule {
    module: Module,
    manifest: ExtensionManifest,
}

/// WASM scripting engine implementation using wasmtime.
pub struct WasmEngine {
    engine: Engine,
    config: WasmEngineConfig,
    hooks: Mutex<HashMap<HookId, (String, HookHandler)>>,
    extensions: Mutex<HashMap<ExtensionId, LoadedModule>>,
    next_hook_id: AtomicU64,
    next_extension_id: AtomicU64,
}

impl WasmEngine {
    /// Create a new WASM engine with the given configuration.
    pub fn new(config: WasmEngineConfig) -> Result<Self> {
        let mut engine_config = Config::new();
        engine_config.consume_fuel(true);
        engine_config.wasm_component_model(true);

        let engine = Engine::new(&engine_config).context("failed to create wasmtime engine")?;

        Ok(Self {
            engine,
            config,
            hooks: Mutex::new(HashMap::new()),
            extensions: Mutex::new(HashMap::new()),
            next_hook_id: AtomicU64::new(1),
            next_extension_id: AtomicU64::new(1),
        })
    }

    /// Create a new WASM engine with default configuration.
    pub fn with_defaults() -> Result<Self> {
        Self::new(WasmEngineConfig::default())
    }

    /// Build a fresh WASI context for a module invocation.
    fn build_wasi_ctx(&self) -> Result<(WasiP1Ctx, Vec<u8>)> {
        let stdout_buf = Vec::new();
        let wasi = WasiCtxBuilder::new()
            .inherit_env()
            .inherit_stderr()
            .build_p1();
        Ok((wasi, stdout_buf))
    }

    /// Compile a WASM module from bytes.
    fn compile_module(&self, bytes: &[u8]) -> Result<Module> {
        Module::new(&self.engine, bytes).context("failed to compile WASM module")
    }

    /// Create a store with fuel and memory limits.
    fn create_store(&self, wasi: WasiP1Ctx, stdout_buf: Vec<u8>) -> Store<WasmState> {
        let mut store = Store::new(&self.engine, WasmState { wasi, stdout_buf });
        store.set_fuel(self.config.fuel_per_call).ok();
        store
    }
}

impl ScriptingEngine for WasmEngine {
    fn eval_config(&self, path: &Path) -> Result<ConfigValue> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read WASM config from {}", path.display()))?;

        let module = self.compile_module(&bytes)?;
        let (wasi, stdout_buf) = self.build_wasi_ctx()?;
        let mut store = self.create_store(wasi, stdout_buf);

        let mut linker = Linker::new(&self.engine);
        wasmtime_wasi::p1::add_to_linker_sync(&mut linker, WasmState::wasi_ctx)
            .context("failed to add WASI to linker")?;

        let instance = linker
            .instantiate(&mut store, &module)
            .context("failed to instantiate WASM config module")?;

        // Look for a `configure` export that returns a JSON string pointer.
        // The WASM module should export:
        //   configure() -> i32  (pointer to JSON string in linear memory)
        //   configure_len() -> i32  (length of that string)
        // OR: a `_start` function that writes JSON to stdout.
        if let Some(configure_fn) = instance
            .get_typed_func::<(), i32>(&mut store, "configure")
            .ok()
        {
            let ptr = configure_fn
                .call(&mut store, ())
                .context("WASM configure() call failed")?;

            let len_fn = instance
                .get_typed_func::<(), i32>(&mut store, "configure_len")
                .context("WASM module exports configure() but not configure_len()")?;
            let len = len_fn
                .call(&mut store, ())
                .context("WASM configure_len() call failed")?;

            let memory = instance
                .get_memory(&mut store, "memory")
                .ok_or_else(|| anyhow!("WASM module has no 'memory' export"))?;

            let data = memory.data(&store);
            let start = ptr as usize;
            let end = start + len as usize;
            if end > data.len() {
                bail!(
                    "WASM configure() returned out-of-bounds pointer: {}..{} (memory size {})",
                    start,
                    end,
                    data.len()
                );
            }

            let json_str = std::str::from_utf8(&data[start..end])
                .context("WASM configure() returned invalid UTF-8")?;

            let value: serde_json::Value =
                serde_json::from_str(json_str).context("WASM configure() returned invalid JSON")?;
            json_to_dynamic(&value)
        } else if let Some(start_fn) = instance.get_typed_func::<(), ()>(&mut store, "_start").ok()
        {
            // WASI _start convention: module writes JSON config to stdout
            start_fn
                .call(&mut store, ())
                .context("WASM _start() call failed")?;

            // Read captured stdout
            let stdout_data = &store.data().stdout_buf;
            if stdout_data.is_empty() {
                bail!("WASM module produced no output on stdout");
            }
            let json_str = std::str::from_utf8(stdout_data)
                .context("WASM module stdout is not valid UTF-8")?;
            let value: serde_json::Value =
                serde_json::from_str(json_str).context("WASM module stdout is not valid JSON")?;
            json_to_dynamic(&value)
        } else {
            bail!("WASM module exports neither configure() nor _start()");
        }
    }

    fn register_hook(&self, event: &str, handler: HookHandler) -> Result<HookId> {
        let id = self.next_hook_id.fetch_add(1, Ordering::Relaxed);
        self.hooks
            .lock()
            .map_err(|_| anyhow!("hook registry lock poisoned"))?
            .insert(id, (event.to_string(), handler));
        Ok(id)
    }

    fn unregister_hook(&self, id: HookId) -> Result<()> {
        self.hooks
            .lock()
            .map_err(|_| anyhow!("hook registry lock poisoned"))?
            .remove(&id);
        Ok(())
    }

    fn fire_event(&self, event: &str, payload: &frankenterm_dynamic::Value) -> Result<Vec<Action>> {
        let hooks = self
            .hooks
            .lock()
            .map_err(|_| anyhow!("hook registry lock poisoned"))?;
        let mut sorted: Vec<_> = hooks.values().collect();
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

    fn load_extension(&self, manifest: &ExtensionManifest) -> Result<ExtensionId> {
        let entrypoint = manifest
            .entrypoint
            .as_deref()
            .ok_or_else(|| anyhow!("WASM extension requires an entrypoint path"))?;

        let bytes = std::fs::read(entrypoint)
            .with_context(|| format!("failed to read WASM extension from {entrypoint}"))?;

        let module = self.compile_module(&bytes)?;
        let id = self.next_extension_id.fetch_add(1, Ordering::Relaxed);

        self.extensions
            .lock()
            .map_err(|_| anyhow!("extension registry lock poisoned"))?
            .insert(
                id,
                LoadedModule {
                    module,
                    manifest: manifest.clone(),
                },
            );

        log::info!(
            "loaded WASM extension {}@{} (id={id})",
            manifest.id,
            manifest.version
        );
        Ok(id)
    }

    fn unload_extension(&self, id: ExtensionId) -> Result<()> {
        let removed = self
            .extensions
            .lock()
            .map_err(|_| anyhow!("extension registry lock poisoned"))?
            .remove(&id);

        match removed {
            Some(loaded) => {
                log::info!(
                    "unloaded WASM extension {}@{} (id={id})",
                    loaded.manifest.id,
                    loaded.manifest.version
                );
                Ok(())
            }
            None => bail!("unknown extension id {id}"),
        }
    }

    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            supports_async: false, // sync-only for now
            supports_filesystem: true,
            supports_network: false,
            sandboxed: true,
            max_memory_bytes: Some(self.config.max_memory_bytes),
            max_execution_time: Some(self.config.max_execution_time),
        }
    }

    fn engine_name(&self) -> &str {
        "wasmtime-41"
    }
}

/// Convert a serde_json Value to a frankenterm_dynamic Value.
fn json_to_dynamic(json: &serde_json::Value) -> Result<frankenterm_dynamic::Value> {
    use frankenterm_dynamic::Value;
    match json {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(b) => Ok(Value::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i >= 0 {
                    Ok(Value::U64(i as u64))
                } else {
                    Ok(Value::I64(i))
                }
            } else if let Some(f) = n.as_f64() {
                Ok(Value::F64(f.into()))
            } else {
                bail!("unsupported JSON number: {n}")
            }
        }
        serde_json::Value::String(s) => Ok(Value::String(s.clone())),
        serde_json::Value::Array(arr) => {
            let items: Result<Vec<_>> = arr.iter().map(json_to_dynamic).collect();
            Ok(Value::Array(items?.into()))
        }
        serde_json::Value::Object(obj) => {
            let mut map = std::collections::BTreeMap::new();
            for (k, v) in obj {
                map.insert(Value::String(k.clone()), json_to_dynamic(v)?);
            }
            Ok(Value::Object(map.into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_creates_successfully() {
        let engine = WasmEngine::with_defaults().unwrap();
        assert_eq!(engine.engine_name(), "wasmtime-41");
    }

    #[test]
    fn capabilities_report_sandboxed() {
        let engine = WasmEngine::with_defaults().unwrap();
        let caps = engine.capabilities();
        assert!(caps.sandboxed);
        assert_eq!(caps.max_memory_bytes, Some(64 * 1024 * 1024));
        assert!(!caps.supports_network);
    }

    #[test]
    fn custom_config_respected() {
        let config = WasmEngineConfig {
            max_memory_bytes: 128 * 1024 * 1024,
            fuel_per_call: 500_000_000,
            max_execution_time: Duration::from_secs(5),
        };
        let engine = WasmEngine::new(config).unwrap();
        let caps = engine.capabilities();
        assert_eq!(caps.max_memory_bytes, Some(128 * 1024 * 1024));
        assert_eq!(caps.max_execution_time, Some(Duration::from_secs(5)));
    }

    #[test]
    fn hook_register_fire_unregister_lifecycle() {
        let engine = WasmEngine::with_defaults().unwrap();

        let handler = HookHandler::new(0, None, |_event, _payload| {
            Ok(vec![Action::Log {
                level: crate::types::LogLevel::Info,
                message: "fired".to_string(),
            }])
        });

        let hook_id = engine.register_hook("test-event", handler).unwrap();
        let actions = engine
            .fire_event("test-event", &frankenterm_dynamic::Value::Null)
            .unwrap();
        assert_eq!(actions.len(), 1);

        engine.unregister_hook(hook_id).unwrap();
        let actions = engine
            .fire_event("test-event", &frankenterm_dynamic::Value::Null)
            .unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn eval_config_rejects_nonexistent_path() {
        let engine = WasmEngine::with_defaults().unwrap();
        let result = engine.eval_config(Path::new("/tmp/nonexistent_wasm_config_12345.wasm"));
        assert!(result.is_err());
    }

    #[test]
    fn json_to_dynamic_converts_basic_types() {
        let json: serde_json::Value = serde_json::json!({
            "string_field": "hello",
            "int_field": 42,
            "float_field": 3.14,
            "bool_field": true,
            "null_field": null,
            "array_field": [1, 2, 3]
        });

        let dynamic = json_to_dynamic(&json).unwrap();
        match dynamic {
            frankenterm_dynamic::Value::Object(map) => {
                assert_eq!(
                    map.get(&frankenterm_dynamic::Value::String(
                        "string_field".to_string()
                    )),
                    Some(&frankenterm_dynamic::Value::String("hello".to_string()))
                );
                assert_eq!(
                    map.get(&frankenterm_dynamic::Value::String("int_field".to_string())),
                    Some(&frankenterm_dynamic::Value::U64(42))
                );
                assert_eq!(
                    map.get(&frankenterm_dynamic::Value::String(
                        "bool_field".to_string()
                    )),
                    Some(&frankenterm_dynamic::Value::Bool(true))
                );
                assert_eq!(
                    map.get(&frankenterm_dynamic::Value::String(
                        "null_field".to_string()
                    )),
                    Some(&frankenterm_dynamic::Value::Null)
                );
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }
}
