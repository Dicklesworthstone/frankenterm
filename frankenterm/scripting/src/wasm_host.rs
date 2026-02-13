//! Host function definitions for WASM extensions.
//!
//! These functions form the FrankenTerm API that WASM extensions can call.
//! Each function is registered with the wasmtime `Linker` and dispatches
//! through the sandbox enforcer for permission checks and audit logging.

use crate::audit::AuditOutcome;
use crate::sandbox::SandboxEnforcer;
use std::sync::Arc;
use wasmtime::{Caller, Linker};

/// State accessible to host functions from within a WASM call.
pub struct HostState {
    /// Sandbox enforcer for permission checks.
    pub enforcer: Arc<SandboxEnforcer>,
    /// WASI context for preview1 support.
    pub wasi: wasmtime_wasi::p1::WasiP1Ctx,
    /// Buffer for data returned from host to WASM.
    pub return_buffer: Vec<u8>,
    /// Log messages collected during this invocation.
    pub log_messages: Vec<(i32, String)>,
}

impl HostState {
    /// Create new host state with the given enforcer and WASI context.
    pub fn new(enforcer: Arc<SandboxEnforcer>, wasi: wasmtime_wasi::p1::WasiP1Ctx) -> Self {
        Self {
            enforcer,
            wasi,
            return_buffer: Vec::new(),
            log_messages: Vec::new(),
        }
    }

    /// Access the WASI context mutably (for p1 linker integration).
    pub fn wasi_ctx(&mut self) -> &mut wasmtime_wasi::p1::WasiP1Ctx {
        &mut self.wasi
    }
}

/// Register all FrankenTerm host functions with the given linker.
pub fn register_host_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // ft_log(level: i32, msg_ptr: i32, msg_len: i32)
    linker.func_wrap(
        "frankenterm",
        "ft_log",
        |mut caller: Caller<'_, HostState>, level: i32, msg_ptr: i32, msg_len: i32| {
            let msg = read_wasm_string(&mut caller, msg_ptr, msg_len).unwrap_or_default();
            caller.data_mut().log_messages.push((level, msg.clone()));
            caller.data().enforcer.record_call(
                "ft_log",
                &format!("level={level} msg={}", truncate(&msg, 80)),
                AuditOutcome::Ok,
            );
        },
    )?;

    // ft_get_env(key_ptr: i32, key_len: i32) -> i32
    // Returns length of value written to return_buffer, or -1 if denied/missing.
    linker.func_wrap(
        "frankenterm",
        "ft_get_env",
        |mut caller: Caller<'_, HostState>, key_ptr: i32, key_len: i32| -> i32 {
            let key = match read_wasm_string(&mut caller, key_ptr, key_len) {
                Some(k) => k,
                None => return -1,
            };

            if caller.data().enforcer.check_env_var(&key).is_err() {
                return -1;
            }

            match std::env::var(&key) {
                Ok(val) => {
                    let bytes = val.into_bytes();
                    let len = bytes.len() as i32;
                    caller.data_mut().return_buffer = bytes;
                    len
                }
                Err(_) => -1,
            }
        },
    )?;

    // ft_return_buffer_read(out_ptr: i32, out_len: i32) -> i32
    // Copy data from host return_buffer into WASM memory.
    // Returns bytes actually written.
    linker.func_wrap(
        "frankenterm",
        "ft_return_buffer_read",
        |mut caller: Caller<'_, HostState>, out_ptr: i32, out_len: i32| -> i32 {
            let buf = caller.data().return_buffer.clone();
            let to_write = buf.len().min(out_len as usize);
            if to_write == 0 {
                return 0;
            }

            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(m)) => m,
                _ => return -1,
            };

            let start = out_ptr as usize;
            let data = memory.data_mut(&mut caller);
            if start + to_write > data.len() {
                return -1;
            }

            data[start..start + to_write].copy_from_slice(&buf[..to_write]);
            to_write as i32
        },
    )?;

    Ok(())
}

/// Read a UTF-8 string from WASM linear memory.
fn read_wasm_string(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> Option<String> {
    if len <= 0 {
        return Some(String::new());
    }

    let memory = match caller.get_export("memory") {
        Some(wasmtime::Extern::Memory(m)) => m,
        _ => return None,
    };

    let start = ptr as usize;
    let end = start + len as usize;
    let data = memory.data(caller);

    if end > data.len() {
        return None;
    }

    std::str::from_utf8(&data[start..end])
        .ok()
        .map(String::from)
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { s } else { &s[..max] }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditTrail;
    use crate::manifest::ExtensionPermissions;
    use crate::sandbox::SandboxConfig;

    fn test_enforcer() -> Arc<SandboxEnforcer> {
        let config = SandboxConfig::from_permissions(
            "test-ext".to_string(),
            ExtensionPermissions {
                environment: vec!["TERM".to_string(), "HOME".to_string()],
                ..Default::default()
            },
        );
        let audit = Arc::new(AuditTrail::new(100));
        Arc::new(SandboxEnforcer::new(config, audit))
    }

    #[test]
    fn host_state_creates() {
        let enforcer = test_enforcer();
        let wasi = wasmtime_wasi::WasiCtxBuilder::new().build_p1();
        let state = HostState::new(enforcer, wasi);
        assert!(state.return_buffer.is_empty());
        assert!(state.log_messages.is_empty());
    }

    #[test]
    fn register_functions_succeeds() {
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        config.wasm_component_model(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let mut linker = Linker::new(&engine);

        // Should not panic
        register_host_functions(&mut linker).unwrap();
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        assert_eq!(truncate("hello world", 5), "hello");
    }
}
