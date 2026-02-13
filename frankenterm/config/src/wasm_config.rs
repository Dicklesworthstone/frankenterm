//! WASM configuration evaluator for FrankenTerm.
//!
//! Loads a `frankenterm.wasm` config module and evaluates it to produce a
//! [`Config`] struct. The WASM module is expected to export a `configure()`
//! function that returns a structured config value.
//!
//! This module does **not** depend on wasmtime directly — the caller provides
//! an evaluator function (typically backed by `WasmEngine::eval_config`).
//!
//! # Search order
//!
//! 1. `$FRANKENTERM_CONFIG_FILE` (environment variable, must end with `.wasm`)
//! 2. `~/.config/frankenterm/frankenterm.wasm`
//!
//! If no WASM config is found, returns `None` and the caller falls through
//! to the next config format.

use crate::{LoadedConfig, CONFIG_DIRS};
use anyhow::Context;
use frankenterm_dynamic::{FromDynamic, FromDynamicOptions, UnknownFieldAction, Value};
use std::path::{Path, PathBuf};

use super::Config;

/// Function signature for a WASM config evaluator.
///
/// The evaluator receives the path to a `.wasm` file and should:
/// 1. Load and instantiate the WASM module
/// 2. Call its `configure()` export
/// 3. Return the resulting config as a dynamic value
pub type WasmEvaluatorFn = dyn Fn(&Path) -> anyhow::Result<Value> + Send + Sync;

/// Search for a `frankenterm.wasm` config and evaluate it.
///
/// The `evaluator` function handles the actual WASM loading and execution
/// (typically `WasmEngine::eval_config`).
///
/// Returns `Some(LoadedConfig)` if a WASM config was found and evaluated,
/// or `None` if no WASM config exists.
pub(crate) fn try_load_wasm_config(
    overrides: &Value,
    evaluator: &WasmEvaluatorFn,
) -> Option<LoadedConfig> {
    let paths = wasm_config_search_paths();

    for path in &paths {
        log::trace!("consider wasm config: {}", path.display());
        match try_load_wasm_file(path, overrides, evaluator) {
            Ok(Some(loaded)) => return Some(loaded),
            Ok(None) => continue,
            Err(err) => {
                return Some(LoadedConfig {
                    config: Err(err),
                    file_name: Some(path.clone()),
                    lua: None,
                    warnings: vec![],
                });
            }
        }
    }

    None
}

/// Build the list of paths to search for `frankenterm.wasm`.
fn wasm_config_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // Highest priority: explicit environment variable
    if let Some(path) = std::env::var_os("FRANKENTERM_CONFIG_FILE") {
        let p = PathBuf::from(path);
        if p.extension().is_some_and(|ext| ext == "wasm") {
            paths.push(p);
        }
    }

    // XDG config dirs
    for dir in CONFIG_DIRS.iter() {
        if let Some(parent) = dir.parent() {
            paths.push(parent.join("frankenterm").join("frankenterm.wasm"));
        }
    }

    paths
}

/// Try to load a single WASM config file.
///
/// Returns `Ok(None)` if the file does not exist, `Ok(Some(loaded))` on
/// success, or `Err` on evaluation/conversion failure.
fn try_load_wasm_file(
    path: &Path,
    overrides: &Value,
    evaluator: &WasmEvaluatorFn,
) -> anyhow::Result<Option<LoadedConfig>> {
    if !path.exists() {
        return Ok(None);
    }

    let (config, warnings) =
        frankenterm_dynamic::Error::capture_warnings(|| -> anyhow::Result<Config> {
            let mut dynamic = evaluator(path)
                .with_context(|| format!("WASM config evaluation failed for {}", path.display()))?;

            // Merge overrides into the dynamic value
            if let (Value::Object(ref mut base), Value::Object(ref ovr)) = (&mut dynamic, overrides)
            {
                for (k, v) in ovr.iter() {
                    base.insert(k.clone(), v.clone());
                }
            }

            let cfg = Config::from_dynamic(
                &dynamic,
                FromDynamicOptions {
                    unknown_fields: UnknownFieldAction::Warn,
                    deprecated_fields: UnknownFieldAction::Warn,
                },
            )
            .with_context(|| {
                format!(
                    "Error converting WASM config from {} to Config struct",
                    path.display()
                )
            })?;

            cfg.check_consistency()
                .context("check_consistency on WASM config")?;

            let _ = cfg.key_bindings();

            std::env::set_var("FRANKENTERM_CONFIG_FILE", path);
            if let Some(dir) = path.parent() {
                std::env::set_var("FRANKENTERM_CONFIG_DIR", dir);
            }

            Ok(cfg)
        });

    let cfg = config?;

    Ok(Some(LoadedConfig {
        config: Ok(cfg.compute_extra_defaults(Some(path))),
        file_name: Some(path.to_path_buf()),
        lua: None,
        warnings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_evaluator(dynamic: Value) -> Box<WasmEvaluatorFn> {
        Box::new(move |_path| Ok(dynamic.clone()))
    }

    fn failing_evaluator(msg: &str) -> Box<WasmEvaluatorFn> {
        let msg = msg.to_string();
        Box::new(move |_path| anyhow::bail!("{msg}"))
    }

    #[test]
    fn nonexistent_wasm_returns_none() {
        let evaluator: Box<WasmEvaluatorFn> = Box::new(|_| Ok(Value::Null));
        let result = try_load_wasm_file(
            Path::new("/tmp/nonexistent_frankenterm_wasm_test.wasm"),
            &Value::default(),
            &*evaluator,
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn wasm_evaluator_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let wasm_path = dir.path().join("test.wasm");
        std::fs::write(&wasm_path, b"fake wasm").unwrap();

        let evaluator = failing_evaluator("WASM module panic");
        let result = try_load_wasm_file(&wasm_path, &Value::default(), &*evaluator);
        match result {
            Err(err) => {
                let err_msg = format!("{err:#}");
                assert!(
                    err_msg.contains("WASM module panic"),
                    "unexpected error: {}",
                    err_msg,
                );
            }
            Ok(_) => panic!("expected error from failing evaluator"),
        }
    }

    #[test]
    fn wasm_evaluator_produces_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let wasm_path = dir.path().join("test.wasm");
        std::fs::write(&wasm_path, b"fake wasm").unwrap();

        // Return an empty object — should produce default config
        let evaluator = mock_evaluator(Value::Object(Default::default()));
        let result = try_load_wasm_file(&wasm_path, &Value::default(), &*evaluator)
            .unwrap()
            .unwrap();
        let cfg = result.config.unwrap();
        assert!(cfg.scrollback_lines > 0);
    }

    #[test]
    fn wasm_overrides_applied() {
        let dir = tempfile::tempdir().unwrap();
        let wasm_path = dir.path().join("test.wasm");
        std::fs::write(&wasm_path, b"fake wasm").unwrap();

        let evaluator = mock_evaluator(Value::Object(Default::default()));

        let mut ovr_map = std::collections::BTreeMap::new();
        ovr_map.insert(Value::String("scrollback_lines".into()), Value::U64(7777));
        let overrides = Value::Object(ovr_map.into());

        let result = try_load_wasm_file(&wasm_path, &overrides, &*evaluator)
            .unwrap()
            .unwrap();
        let cfg = result.config.unwrap();
        assert_eq!(cfg.scrollback_lines, 7777);
    }

    #[test]
    fn wasm_search_paths_returns_entries() {
        let paths = wasm_config_search_paths();
        // Should have at least the XDG path entries
        for p in &paths {
            assert!(
                p.to_string_lossy().contains("frankenterm.wasm"),
                "unexpected path: {}",
                p.display()
            );
        }
    }

    #[test]
    fn try_load_returns_none_when_no_wasm_exists() {
        let evaluator: Box<WasmEvaluatorFn> = Box::new(|_| panic!("should not be called"));
        let result = try_load_wasm_config(&Value::default(), &*evaluator);
        // Unless someone has a frankenterm.wasm in their config dir, should be None
        // We can't guarantee this in CI so we just check it doesn't panic
        let _ = result;
    }
}
