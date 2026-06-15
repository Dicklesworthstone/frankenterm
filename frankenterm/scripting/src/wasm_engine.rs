//! WASM scripting engine backed by wasmtime.
//!
//! Implements [`ScriptingEngine`] using wasmtime's runtime with WASI
//! support for config evaluation and (eventually) capability-gated extensions.
//!
//! # Security model — what is ACTUALLY enforced (ft-0dki4)
//!
//! Each WASM module runs in a `Store` with:
//! - **Memory limit**: enforced via a [`StoreLimits`] resource limiter capping
//!   linear-memory growth at `max_memory_bytes` (default 64 MiB per module).
//! - **Fuel metering**: enforced instruction budget (default 1 billion). This
//!   is the only CPU bound — see the wall-clock caveat below.
//! - **No host environment leak**: the guest WASI environment is empty; host
//!   env vars (including secrets) are NOT inherited. stdout is captured;
//!   stderr is inherited (output only).
//! - **No filesystem access**: no directories are preopened, so the guest has
//!   no host filesystem capability.
//!
//! # NOT yet enforced (deferred to ft-rxk40, the crate-wiring effort)
//!
//! Do not rely on these — they are intentionally absent until the engine is
//! wired:
//! - **Wall-clock timeout**: `max_execution_time` is reported in
//!   [`EngineCapabilities`] but NOT enforced. A host-call or busy stall that
//!   does not burn fuel has no wall-clock bound (no epoch interruption /
//!   watchdog). Only fuel-based CPU limiting applies.
//! - **Capability / permission enforcement**: the manifest `[permissions]`
//!   allow-list and the [`crate::sandbox::SandboxEnforcer`] + `ft_*` host API
//!   (`wasm_host`) are NOT registered into this engine's linker — the engine
//!   only adds the WASI p1 linker. Manifest permissions are not consulted at
//!   runtime here.
//! - **Manifest environment allow-list**: not plumbed into the WASI context
//!   (the guest simply gets no env at all, which is the safe default).

use crate::ScriptingEngine;
use crate::types::{
    Action, ConfigValue, EngineCapabilities, ExtensionId, ExtensionManifest, HookHandler, HookId,
};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use wasmtime::*;
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

const MAX_STDOUT_CAPTURE_BYTES: usize = 1024 * 1024;

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
    stdout: MemoryOutputPipe,
    /// Per-store resource limiter enforcing the memory cap (ft-0dki4).
    limits: StoreLimits,
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

        let engine = Engine::new(&engine_config)
            .map_err(|err| err.context("failed to create wasmtime engine"))?;

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
    fn build_wasi_ctx(&self) -> Result<(WasiP1Ctx, MemoryOutputPipe)> {
        let stdout = MemoryOutputPipe::new(MAX_STDOUT_CAPTURE_BYTES);
        // ft-0dki4: do NOT inherit the host environment. `inherit_env()` copied
        // every host env var (including secrets) into the guest WASI environ,
        // readable via `environ_get` — a host→guest confidentiality leak that
        // ignored the manifest environment allow-list. The guest gets no env;
        // the manifest allow-list will be plumbed here when the capability
        // enforcer is wired (ft-rxk40).
        let wasi = WasiCtxBuilder::new()
            .stdout(stdout.clone())
            .inherit_stderr()
            .build_p1();
        Ok((wasi, stdout))
    }

    /// Compile a WASM module from bytes.
    fn compile_module(&self, bytes: &[u8]) -> Result<Module> {
        Ok(Module::new(&self.engine, bytes)
            .map_err(|err| err.context("failed to compile WASM module"))?)
    }

    /// Create a store with enforced fuel and memory limits.
    fn create_store(&self, wasi: WasiP1Ctx, stdout: MemoryOutputPipe) -> Store<WasmState> {
        // ft-0dki4: enforce the advertised per-module memory cap. Without a
        // ResourceLimiter a module could grow linear memory to the wasm32
        // 4 GiB ceiling regardless of `max_memory_bytes`.
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.config.max_memory_bytes)
            .build();
        let mut store = Store::new(
            &self.engine,
            WasmState {
                wasi,
                stdout,
                limits,
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_fuel(self.config.fuel_per_call).ok();
        store
    }

    fn lock_or_recover<'a, T>(mutex: &'a Mutex<T>, registry: &str) -> MutexGuard<'a, T> {
        match mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                log::warn!("recovering poisoned WASM engine {registry} registry lock");
                mutex.clear_poison();
                poisoned.into_inner()
            }
        }
    }

    fn hooks_guard(&self) -> MutexGuard<'_, HashMap<HookId, (String, HookHandler)>> {
        Self::lock_or_recover(&self.hooks, "hook")
    }

    fn extensions_guard(&self) -> MutexGuard<'_, HashMap<ExtensionId, LoadedModule>> {
        Self::lock_or_recover(&self.extensions, "extension")
    }
}

impl ScriptingEngine for WasmEngine {
    fn eval_config(&self, path: &Path) -> Result<ConfigValue> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read WASM config from {}", path.display()))?;

        let module = self.compile_module(&bytes)?;
        let (wasi, stdout) = self.build_wasi_ctx()?;
        let mut store = self.create_store(wasi, stdout);

        let mut linker = Linker::new(&self.engine);
        wasmtime_wasi::p1::add_to_linker_sync(&mut linker, WasmState::wasi_ctx)
            .map_err(|err| err.context("failed to add WASI to linker"))?;

        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|err| err.context("failed to instantiate WASM config module"))?;

        // Look for a `configure` export that returns a JSON string pointer.
        // The WASM module should export:
        //   configure() -> i32  (pointer to JSON string in linear memory)
        //   configure_len() -> i32  (length of that string)
        // OR: a `_start` function that writes JSON to stdout.
        if let Ok(configure_fn) = instance.get_typed_func::<(), i32>(&mut store, "configure") {
            let ptr = configure_fn
                .call(&mut store, ())
                .map_err(|err| err.context("WASM configure() call failed"))?;

            let len_fn = instance
                .get_typed_func::<(), i32>(&mut store, "configure_len")
                .map_err(|err| {
                    err.context("WASM module exports configure() but not configure_len()")
                })?;
            let len = len_fn
                .call(&mut store, ())
                .map_err(|err| err.context("WASM configure_len() call failed"))?;

            let memory = instance
                .get_memory(&mut store, "memory")
                .ok_or_else(|| anyhow!("WASM module has no 'memory' export"))?;

            let data = memory.data(&store);
            if ptr < 0 || len < 0 {
                bail!("WASM configure() returned negative pointer/length: {ptr}/{len}");
            }
            let start = usize::try_from(ptr).context("WASM configure() pointer out of range")?;
            let len = usize::try_from(len).context("WASM configure() length out of range")?;
            let end = start.checked_add(len).ok_or_else(|| {
                anyhow!("WASM configure() pointer range overflowed: {start}+{len}")
            })?;
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
        } else if let Ok(start_fn) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
            // WASI _start convention: module writes JSON config to stdout
            start_fn
                .call(&mut store, ())
                .map_err(|err| err.context("WASM _start() call failed"))?;

            // Read captured stdout
            let stdout_data = store.data().stdout.contents();
            if stdout_data.is_empty() {
                bail!("WASM module produced no output on stdout");
            }
            let json_str = std::str::from_utf8(stdout_data.as_ref())
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
        self.hooks_guard().insert(id, (event.to_string(), handler));
        Ok(id)
    }

    fn unregister_hook(&self, id: HookId) -> Result<()> {
        self.hooks_guard().remove(&id);
        Ok(())
    }

    fn fire_event(&self, event: &str, payload: &frankenterm_dynamic::Value) -> Result<Vec<Action>> {
        let mut sorted = self
            .hooks_guard()
            .values()
            .filter(|(registered_event, handler)| {
                registered_event == event && handler.matches_event(event)
            })
            .map(|(_, handler)| handler.clone())
            .collect::<Vec<_>>();
        sorted.sort_by_key(|handler| handler.priority);

        let mut actions = Vec::new();
        for handler in sorted {
            let mut from_handler = handler.call(event, payload)?;
            actions.append(&mut from_handler);
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

        self.extensions_guard().insert(
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
        let removed = self.extensions_guard().remove(&id);

        match removed {
            Some(loaded) => {
                let LoadedModule { module, manifest } = loaded;
                let export_count = module.exports().count();
                log::info!(
                    "unloaded WASM extension {}@{} (id={id}, exports={export_count})",
                    manifest.id,
                    manifest.version
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
        "wasmtime-45"
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

    const WASI_SNAPSHOT_PREVIEW1: &str = "wasi_snapshot_preview1";
    const FD_WRITE: &str = "fd_write";

    fn push_uleb_u32(out: &mut Vec<u8>, mut value: u32) {
        loop {
            let mut byte = u8::try_from(value & 0x7f).expect("ULEB128 chunk fits in u8");
            value >>= 7;
            if value == 0 {
                out.push(byte);
                break;
            }
            byte |= 0x80;
            out.push(byte);
        }
    }

    fn push_len(out: &mut Vec<u8>, len: usize, what: &str) {
        let len = u32::try_from(len).expect(what);
        push_uleb_u32(out, len);
    }

    fn push_name(out: &mut Vec<u8>, name: &str) {
        push_len(out, name.len(), "WASM name length fits in u32");
        out.extend_from_slice(name.as_bytes());
    }

    fn push_section(module: &mut Vec<u8>, section_id: u8, payload: &[u8]) {
        module.push(section_id);
        push_len(module, payload.len(), "WASM section length fits in u32");
        module.extend_from_slice(payload);
    }

    fn wasm_start_writes_stdout_json(json: &str) -> Vec<u8> {
        let json_bytes = json.as_bytes();
        let json_offset = 16u32;
        let written_count_offset = 8u32;
        let json_len = u32::try_from(json_bytes.len()).expect("test JSON length fits in u32");

        let mut module = b"\0asm\x01\0\0\0".to_vec();

        let mut types = Vec::new();
        push_uleb_u32(&mut types, 2);
        types.push(0x60);
        push_uleb_u32(&mut types, 4);
        types.extend_from_slice(&[0x7f, 0x7f, 0x7f, 0x7f]);
        push_uleb_u32(&mut types, 1);
        types.push(0x7f);
        types.push(0x60);
        push_uleb_u32(&mut types, 0);
        push_uleb_u32(&mut types, 0);
        push_section(&mut module, 1, &types);

        let mut imports = Vec::new();
        push_uleb_u32(&mut imports, 1);
        push_name(&mut imports, WASI_SNAPSHOT_PREVIEW1);
        push_name(&mut imports, FD_WRITE);
        imports.push(0x00);
        push_uleb_u32(&mut imports, 0);
        push_section(&mut module, 2, &imports);

        let mut functions = Vec::new();
        push_uleb_u32(&mut functions, 1);
        push_uleb_u32(&mut functions, 1);
        push_section(&mut module, 3, &functions);

        let mut memories = Vec::new();
        push_uleb_u32(&mut memories, 1);
        memories.push(0x00);
        push_uleb_u32(&mut memories, 1);
        push_section(&mut module, 5, &memories);

        let mut exports = Vec::new();
        push_uleb_u32(&mut exports, 2);
        push_name(&mut exports, "memory");
        exports.push(0x02);
        push_uleb_u32(&mut exports, 0);
        push_name(&mut exports, "_start");
        exports.push(0x00);
        push_uleb_u32(&mut exports, 1);
        push_section(&mut module, 7, &exports);

        let mut body = Vec::new();
        push_uleb_u32(&mut body, 0);
        for value in [1, 0, 1, written_count_offset] {
            body.push(0x41);
            push_uleb_u32(&mut body, value);
        }
        body.push(0x10);
        push_uleb_u32(&mut body, 0);
        body.push(0x1a);
        body.push(0x0b);

        let mut code = Vec::new();
        push_uleb_u32(&mut code, 1);
        push_len(&mut code, body.len(), "WASM body length fits in u32");
        code.extend_from_slice(&body);
        push_section(&mut module, 10, &code);

        let mut iovec = Vec::new();
        iovec.extend_from_slice(&json_offset.to_le_bytes());
        iovec.extend_from_slice(&json_len.to_le_bytes());

        let mut data = Vec::new();
        push_uleb_u32(&mut data, 2);
        data.push(0x00);
        data.push(0x41);
        push_uleb_u32(&mut data, 0);
        data.push(0x0b);
        push_len(&mut data, iovec.len(), "WASM iovec length fits in u32");
        data.extend_from_slice(&iovec);
        data.push(0x00);
        data.push(0x41);
        push_uleb_u32(&mut data, json_offset);
        data.push(0x0b);
        push_len(&mut data, json_bytes.len(), "WASM JSON length fits in u32");
        data.extend_from_slice(json_bytes);
        push_section(&mut module, 11, &data);

        module
    }

    #[test]
    fn engine_creates_successfully() {
        let engine = WasmEngine::with_defaults().unwrap();
        assert_eq!(engine.engine_name(), "wasmtime-45");
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
    fn hook_registry_recovers_after_poisoned_lock() {
        let engine = WasmEngine::with_defaults().unwrap();

        let poison_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = engine.hooks.lock().unwrap();
            std::panic::panic_any("poison WASM hook registry");
        }));
        assert!(poison_result.is_err());
        assert!(engine.hooks.is_poisoned());

        let handler = HookHandler::new(0, Some("test-event".to_string()), |_event, _payload| {
            Ok(vec![Action::Log {
                level: crate::types::LogLevel::Info,
                message: "recovered".to_string(),
            }])
        });

        let hook_id = engine.register_hook("test-event", handler).unwrap();
        assert!(!engine.hooks.is_poisoned());

        let actions = engine
            .fire_event("test-event", &frankenterm_dynamic::Value::Null)
            .unwrap();
        assert_eq!(actions.len(), 1);

        engine.unregister_hook(hook_id).unwrap();
        let actions_after_unreg = engine
            .fire_event("test-event", &frankenterm_dynamic::Value::Null)
            .unwrap();
        assert!(actions_after_unreg.is_empty());
    }

    #[test]
    fn fire_event_drops_hook_registry_lock_before_calling_handlers() {
        let engine = std::sync::Arc::new(WasmEngine::with_defaults().unwrap());
        let engine_for_handler = std::sync::Arc::clone(&engine);

        let handler = HookHandler::new(
            0,
            Some("test-event".to_string()),
            move |_event, _payload| {
                let _guard = engine_for_handler
                    .hooks
                    .try_lock()
                    .expect("fire_event must not hold hook registry while invoking handlers");
                Ok(vec![Action::Log {
                    level: crate::types::LogLevel::Info,
                    message: "unlocked".to_string(),
                }])
            },
        );

        engine.register_hook("test-event", handler).unwrap();
        let actions = engine
            .fire_event("test-event", &frankenterm_dynamic::Value::Null)
            .unwrap();
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn eval_config_rejects_nonexistent_path() {
        let engine = WasmEngine::with_defaults().unwrap();
        let result = engine.eval_config(Path::new("/tmp/nonexistent_wasm_config_12345.wasm"));
        assert!(result.is_err());
    }

    #[test]
    fn load_extension_missing_entrypoint_errors() {
        let engine = WasmEngine::with_defaults().unwrap();
        let manifest = ExtensionManifest {
            id: "test-ext".to_string(),
            version: "1.0.0".to_string(),
            entrypoint: None,
            metadata: frankenterm_dynamic::Value::Null,
        };
        let result = engine.load_extension(&manifest);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("entrypoint"));
    }

    #[test]
    fn load_extension_nonexistent_file_errors() {
        let engine = WasmEngine::with_defaults().unwrap();
        let manifest = ExtensionManifest {
            id: "missing".to_string(),
            version: "1.0.0".to_string(),
            entrypoint: Some("/tmp/nonexistent_wasm_ext_99999.wasm".to_string()),
            metadata: frankenterm_dynamic::Value::Null,
        };
        let result = engine.load_extension(&manifest);
        assert!(result.is_err());
    }

    #[test]
    fn load_extension_invalid_wasm_binary_errors() {
        let engine = WasmEngine::with_defaults().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let wasm_path = dir.path().join("bad.wasm");
        std::fs::write(&wasm_path, b"not valid wasm bytes").unwrap();

        let manifest = ExtensionManifest {
            id: "bad-wasm".to_string(),
            version: "1.0.0".to_string(),
            entrypoint: Some(wasm_path.to_string_lossy().into_owned()),
            metadata: frankenterm_dynamic::Value::Null,
        };
        let result = engine.load_extension(&manifest);
        assert!(result.is_err());
    }

    #[test]
    fn extension_registry_recovers_after_poisoned_lock() {
        let engine = WasmEngine::with_defaults().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let wasm_path = dir.path().join("empty.wasm");
        std::fs::write(&wasm_path, b"\0asm\x01\0\0\0").unwrap();

        let poison_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = engine.extensions.lock().unwrap();
            std::panic::panic_any("poison WASM extension registry");
        }));
        assert!(poison_result.is_err());
        assert!(engine.extensions.is_poisoned());

        let manifest = ExtensionManifest {
            id: "recovered".to_string(),
            version: "1.0.0".to_string(),
            entrypoint: Some(wasm_path.to_string_lossy().into_owned()),
            metadata: frankenterm_dynamic::Value::Null,
        };

        let extension_id = engine.load_extension(&manifest).unwrap();
        assert!(!engine.extensions.is_poisoned());
        assert_eq!(engine.extensions.lock().unwrap().len(), 1);

        engine.unload_extension(extension_id).unwrap();
        assert!(engine.extensions.lock().unwrap().is_empty());
    }

    #[test]
    fn eval_config_invalid_wasm_binary_errors() {
        let engine = WasmEngine::with_defaults().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let wasm_path = dir.path().join("bad_config.wasm");
        std::fs::write(&wasm_path, b"not a wasm module").unwrap();

        let result = engine.eval_config(&wasm_path);
        assert!(result.is_err());
    }

    #[test]
    fn eval_config_accepts_wasi_start_stdout_json() {
        let engine = WasmEngine::with_defaults().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let wasm_path = dir.path().join("stdout_config.wasm");
        std::fs::write(
            &wasm_path,
            wasm_start_writes_stdout_json(r#"{"from":"stdout","answer":42}"#),
        )
        .unwrap();

        let config = engine.eval_config(&wasm_path).unwrap();

        match config {
            frankenterm_dynamic::Value::Object(map) => {
                assert_eq!(
                    map.get(&frankenterm_dynamic::Value::String("from".to_string())),
                    Some(&frankenterm_dynamic::Value::String("stdout".to_string()))
                );
                assert_eq!(
                    map.get(&frankenterm_dynamic::Value::String("answer".to_string())),
                    Some(&frankenterm_dynamic::Value::U64(42))
                );
            }
            other => std::panic::panic_any(format!("expected object config, got {other:?}")),
        }
    }

    #[test]
    fn wasm_memory_limit_is_enforced_ft_0dki4() {
        // ft-0dki4: the advertised per-module memory cap must be ENFORCED by the
        // StoreLimits resource limiter installed in create_store. A module
        // declaring more linear memory than `max_memory_bytes` must be rejected
        // at instantiation; without the limiter wasmtime honors the declaration
        // up to the wasm32 4 GiB ceiling.
        fn module_with_memory_min(pages: u32) -> Vec<u8> {
            let mut module = b"\0asm\x01\0\0\0".to_vec();
            let mut mem = Vec::new();
            push_uleb_u32(&mut mem, 1); // one memory
            mem.push(0x00); // limits flag: min only
            push_uleb_u32(&mut mem, pages);
            push_section(&mut module, 5, &mem);
            module
        }

        let engine = WasmEngine::with_defaults().unwrap(); // 64 MiB cap = 1024 pages
        let dir = tempfile::tempdir().unwrap();

        // 2048 pages = 128 MiB > 64 MiB cap -> rejected by the limiter.
        let big = dir.path().join("big_mem.wasm");
        std::fs::write(&big, module_with_memory_min(2048)).unwrap();
        let big_err = engine
            .eval_config(&big)
            .expect_err("oversized-memory module must be rejected");
        let big_msg = format!("{big_err:?}").to_lowercase();
        assert!(
            big_msg.contains("memor") || big_msg.contains("limit") || big_msg.contains("exceed"),
            "oversized module must fail on the memory limiter, got: {big_msg}"
        );

        // Control: an under-cap module gets PAST the limiter (it then fails only
        // for lacking configure()/_start()). This proves the limiter rejects on
        // size, not unconditionally — and would FAIL if the limiter were dropped
        // (the big module would then also reach the missing-exports path).
        let small = dir.path().join("small_mem.wasm");
        std::fs::write(&small, module_with_memory_min(1)).unwrap();
        let small_err = engine
            .eval_config(&small)
            .expect_err("module without exports must error");
        let small_msg = format!("{small_err:?}").to_lowercase();
        assert!(
            !small_msg.contains("memor")
                && !small_msg.contains("limit")
                && !small_msg.contains("exceed"),
            "under-cap module must pass the memory limiter, got: {small_msg}"
        );
    }

    #[test]
    fn wasm_engine_does_not_leak_host_env_ft_0dki4() {
        // ft-0dki4 regression guard: the WASI context must not inherit the host
        // environment (it copied host secrets into the guest environ), and the
        // store must install a memory limiter. Source-level so it holds without
        // a WAT toolchain dependency.
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/wasm_engine.rs"
        ))
        .expect("read wasm_engine.rs source");
        // Needles are assembled from fragments so this guard does not match its
        // own assertion text — only genuine production callsites count.
        let inherit_env_call = concat!(".inherit", "_env(");
        let limiter_call = concat!(".limit", "er(");
        let limits_builder = concat!("StoreLimits", "Builder");
        let memory_size_call = concat!("memory", "_size(");
        assert!(
            !src.contains(inherit_env_call),
            "ft-0dki4 regression: WASI ctx must NOT inherit the host environment \
             (leaks host secrets into the guest via environ_get)"
        );
        assert!(
            src.contains(limiter_call)
                && src.contains(limits_builder)
                && src.contains(memory_size_call),
            "ft-0dki4 regression: create_store must install a StoreLimits memory limiter \
             capping memory_size"
        );
    }

    #[test]
    fn unload_nonexistent_extension_errors() {
        let engine = WasmEngine::with_defaults().unwrap();
        let result = engine.unload_extension(9999);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown extension")
        );
    }

    #[test]
    fn multiple_hooks_fire_in_priority_order() {
        let engine = WasmEngine::with_defaults().unwrap();

        let handler_high = HookHandler::new(10, Some("evt".to_string()), |_, _| {
            Ok(vec![Action::Custom {
                name: "high".to_string(),
                payload: frankenterm_dynamic::Value::Null,
            }])
        });
        let handler_low = HookHandler::new(-5, Some("evt".to_string()), |_, _| {
            Ok(vec![Action::Custom {
                name: "low".to_string(),
                payload: frankenterm_dynamic::Value::Null,
            }])
        });

        engine.register_hook("evt", handler_high).unwrap();
        engine.register_hook("evt", handler_low).unwrap();

        let actions = engine
            .fire_event("evt", &frankenterm_dynamic::Value::Null)
            .unwrap();
        assert_eq!(actions.len(), 2);
        // Low priority (-5) fires before high priority (10)
        match &actions[0] {
            Action::Custom { name, .. } => assert_eq!(name, "low"),
            other => std::panic::panic_any(format!("expected Custom, got {other:?}")),
        }
        match &actions[1] {
            Action::Custom { name, .. } => assert_eq!(name, "high"),
            other => std::panic::panic_any(format!("expected Custom, got {other:?}")),
        }
    }

    #[test]
    fn hook_filter_prevents_non_matching_events() {
        let engine = WasmEngine::with_defaults().unwrap();
        let handler = HookHandler::new(0, Some("resize".to_string()), |_, _| {
            Ok(vec![Action::Custom {
                name: "fired".to_string(),
                payload: frankenterm_dynamic::Value::Null,
            }])
        });
        engine.register_hook("resize", handler).unwrap();

        let actions = engine
            .fire_event("pane-output", &frankenterm_dynamic::Value::Null)
            .unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn json_to_dynamic_negative_integer() {
        let json: serde_json::Value = serde_json::json!(-42);
        let dynamic = json_to_dynamic(&json).unwrap();
        assert_eq!(dynamic, frankenterm_dynamic::Value::I64(-42));
    }

    #[test]
    fn json_to_dynamic_nested_objects() {
        let json: serde_json::Value = serde_json::json!({
            "outer": {
                "inner": [1, 2]
            }
        });
        let dynamic = json_to_dynamic(&json).unwrap();
        match dynamic {
            frankenterm_dynamic::Value::Object(map) => {
                let outer = map
                    .get(&frankenterm_dynamic::Value::String("outer".to_string()))
                    .unwrap();
                match outer {
                    frankenterm_dynamic::Value::Object(inner_map) => {
                        assert!(
                            inner_map
                                .get(&frankenterm_dynamic::Value::String("inner".to_string()))
                                .is_some()
                        );
                    }
                    other => std::panic::panic_any(format!("expected Object, got {other:?}")),
                }
            }
            other => std::panic::panic_any(format!("expected Object, got {other:?}")),
        }
    }

    #[test]
    fn json_to_dynamic_null_and_bool() {
        assert_eq!(
            json_to_dynamic(&serde_json::Value::Null).unwrap(),
            frankenterm_dynamic::Value::Null
        );
        assert_eq!(
            json_to_dynamic(&serde_json::json!(true)).unwrap(),
            frankenterm_dynamic::Value::Bool(true)
        );
        assert_eq!(
            json_to_dynamic(&serde_json::json!(false)).unwrap(),
            frankenterm_dynamic::Value::Bool(false)
        );
    }

    #[test]
    fn json_to_dynamic_empty_array_and_object() {
        let arr = json_to_dynamic(&serde_json::json!([])).unwrap();
        match arr {
            frankenterm_dynamic::Value::Array(a) => assert!(a.is_empty()),
            other => std::panic::panic_any(format!("expected Array, got {other:?}")),
        }

        let obj = json_to_dynamic(&serde_json::json!({})).unwrap();
        match obj {
            frankenterm_dynamic::Value::Object(m) => assert!(m.is_empty()),
            other => std::panic::panic_any(format!("expected Object, got {other:?}")),
        }
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
            other => std::panic::panic_any(format!("expected Object, got {other:?}")),
        }
    }
}
