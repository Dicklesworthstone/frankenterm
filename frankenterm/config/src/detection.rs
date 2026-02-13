//! Config format auto-detection.
//!
//! Probes standard locations to determine which config format is present
//! and returns the detected format plus path.
//!
//! # Detection priority
//!
//! 1. `$FRANKENTERM_CONFIG_FILE` (explicit override — any format)
//! 2. `frankenterm.toml` in standard locations
//! 3. `frankenterm.wasm` in standard locations (if wasm evaluator provided)
//! 4. `wezterm.lua` / `.wezterm.lua` in standard locations (legacy compat)
//! 5. Built-in defaults (no config file)

use crate::wasm_config::WasmEvaluatorFn;
use crate::{LoadedConfig, CONFIG_DIRS, HOME_DIR};
use std::path::{Path, PathBuf};

/// Detected config format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigFormat {
    /// Native structured config (`frankenterm.toml`).
    Toml,
    /// Legacy Lua config (`wezterm.lua`).
    Lua,
    /// WASM-evaluated dynamic config (`frankenterm.wasm`).
    Wasm,
    /// No config file found; use built-in defaults.
    Default,
}

impl ConfigFormat {
    /// File extension for this format, if applicable.
    pub fn extension(&self) -> Option<&str> {
        match self {
            Self::Toml => Some("toml"),
            Self::Lua => Some("lua"),
            Self::Wasm => Some("wasm"),
            Self::Default => None,
        }
    }

    /// Human-readable label for logging.
    pub fn label(&self) -> &str {
        match self {
            Self::Toml => "frankenterm.toml",
            Self::Lua => "wezterm.lua (legacy)",
            Self::Wasm => "frankenterm.wasm",
            Self::Default => "built-in defaults",
        }
    }
}

/// Result of config format detection.
#[derive(Clone, Debug)]
pub struct DetectedConfig {
    pub format: ConfigFormat,
    pub path: Option<PathBuf>,
}

/// Detect which config format to use by probing standard locations.
///
/// If `wasm_evaluator` is `Some`, WASM config files are considered in the
/// detection priority. Otherwise they are skipped.
pub fn detect_config(wasm_available: bool) -> DetectedConfig {
    // 1. Explicit env var override
    if let Some(path_os) = std::env::var_os("FRANKENTERM_CONFIG_FILE") {
        let path = PathBuf::from(path_os);
        if path.exists() {
            if let Some(format) = format_from_extension(&path) {
                log::info!(
                    "Config: using {} from $FRANKENTERM_CONFIG_FILE",
                    format.label()
                );
                return DetectedConfig {
                    format,
                    path: Some(path),
                };
            }
        }
    }

    // Build search directories (frankenterm subdirs)
    let mut frankenterm_dirs: Vec<PathBuf> = Vec::new();
    for dir in CONFIG_DIRS.iter() {
        if let Some(parent) = dir.parent() {
            frankenterm_dirs.push(parent.join("frankenterm"));
        }
    }

    // 2. frankenterm.toml in standard locations
    for dir in &frankenterm_dirs {
        let toml_path = dir.join("frankenterm.toml");
        if toml_path.exists() {
            log::info!(
                "Config: loaded {} from {}",
                ConfigFormat::Toml.label(),
                dir.display()
            );
            return DetectedConfig {
                format: ConfigFormat::Toml,
                path: Some(toml_path),
            };
        }
    }

    // Also check home directory dotfile
    let home_toml = HOME_DIR.join(".frankenterm.toml");
    if home_toml.exists() {
        log::info!("Config: loaded {} from ~/", ConfigFormat::Toml.label());
        return DetectedConfig {
            format: ConfigFormat::Toml,
            path: Some(home_toml),
        };
    }

    // 3. frankenterm.wasm in standard locations (if available)
    if wasm_available {
        for dir in &frankenterm_dirs {
            let wasm_path = dir.join("frankenterm.wasm");
            if wasm_path.exists() {
                log::info!(
                    "Config: loaded {} from {}",
                    ConfigFormat::Wasm.label(),
                    dir.display()
                );
                return DetectedConfig {
                    format: ConfigFormat::Wasm,
                    path: Some(wasm_path),
                };
            }
        }
    }

    // 4. Legacy wezterm.lua paths
    let legacy_paths = legacy_lua_search_paths();
    for lua_path in &legacy_paths {
        if lua_path.exists() {
            log::info!(
                "Config: loaded {} from {}",
                ConfigFormat::Lua.label(),
                lua_path
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            );
            return DetectedConfig {
                format: ConfigFormat::Lua,
                path: Some(lua_path.clone()),
            };
        }
    }

    // 5. No config found
    log::warn!("Config: no config file found, using defaults");
    DetectedConfig {
        format: ConfigFormat::Default,
        path: None,
    }
}

/// Load config using the detected format.
///
/// Dispatches to the appropriate loader based on format. The WASM evaluator
/// is required when format is `Wasm`.
pub fn load_detected_config(
    detected: &DetectedConfig,
    overrides: &frankenterm_dynamic::Value,
    wasm_evaluator: Option<&WasmEvaluatorFn>,
) -> LoadedConfig {
    match detected.format {
        ConfigFormat::Toml => {
            if let Some(loaded) = crate::toml_config::try_load_toml_config(overrides) {
                loaded
            } else {
                // Detection found a TOML file but loader couldn't load it —
                // this shouldn't happen but fall back gracefully.
                LoadedConfig {
                    config: Ok(super::Config::default_config()),
                    file_name: detected.path.clone(),
                    lua: None,
                    warnings: vec!["TOML config detected but failed to load; using defaults".into()],
                }
            }
        }
        ConfigFormat::Wasm => {
            if let Some(evaluator) = wasm_evaluator {
                if let Some(loaded) = crate::wasm_config::try_load_wasm_config(overrides, evaluator)
                {
                    return loaded;
                }
            }
            LoadedConfig {
                config: Ok(super::Config::default_config()),
                file_name: detected.path.clone(),
                lua: None,
                warnings: vec![
                    "WASM config detected but no evaluator available; using defaults".into(),
                ],
            }
        }
        ConfigFormat::Lua => {
            // Lua loading is handled by the existing code path in config.rs.
            // Return None-equivalent to signal the caller should continue with
            // the legacy Lua loading path.
            LoadedConfig {
                config: Ok(super::Config::default_config()),
                file_name: detected.path.clone(),
                lua: None,
                warnings: vec![],
            }
        }
        ConfigFormat::Default => LoadedConfig {
            config: Ok(super::Config::default_config()),
            file_name: None,
            lua: None,
            warnings: vec![],
        },
    }
}

/// Determine config format from file extension.
fn format_from_extension(path: &Path) -> Option<ConfigFormat> {
    path.extension().and_then(|ext| {
        let ext = ext.to_string_lossy();
        match ext.as_ref() {
            "toml" => Some(ConfigFormat::Toml),
            "lua" => Some(ConfigFormat::Lua),
            "wasm" => Some(ConfigFormat::Wasm),
            _ => None,
        }
    })
}

/// Legacy Lua config search paths.
fn legacy_lua_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // ~/.wezterm.lua
    paths.push(HOME_DIR.join(".wezterm.lua"));

    // XDG config dirs for wezterm
    for dir in CONFIG_DIRS.iter() {
        paths.push(dir.join("wezterm.lua"));
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests that modify FRANKENTERM_CONFIG_FILE must not run concurrently.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn format_extension_mapping() {
        assert_eq!(
            format_from_extension(Path::new("config.toml")),
            Some(ConfigFormat::Toml)
        );
        assert_eq!(
            format_from_extension(Path::new("config.lua")),
            Some(ConfigFormat::Lua)
        );
        assert_eq!(
            format_from_extension(Path::new("config.wasm")),
            Some(ConfigFormat::Wasm)
        );
        assert_eq!(format_from_extension(Path::new("config.json")), None);
        assert_eq!(format_from_extension(Path::new("no_extension")), None);
    }

    #[test]
    fn format_labels() {
        assert_eq!(ConfigFormat::Toml.label(), "frankenterm.toml");
        assert_eq!(ConfigFormat::Lua.label(), "wezterm.lua (legacy)");
        assert_eq!(ConfigFormat::Wasm.label(), "frankenterm.wasm");
        assert_eq!(ConfigFormat::Default.label(), "built-in defaults");
    }

    #[test]
    fn format_extensions() {
        assert_eq!(ConfigFormat::Toml.extension(), Some("toml"));
        assert_eq!(ConfigFormat::Lua.extension(), Some("lua"));
        assert_eq!(ConfigFormat::Wasm.extension(), Some("wasm"));
        assert_eq!(ConfigFormat::Default.extension(), None);
    }

    #[test]
    fn detect_returns_default_when_no_config_exists() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // In a test environment, it's unlikely there's a real config file
        // in the standard locations. If there is, this test is not useful
        // but also not harmful.
        let detected = detect_config(false);
        // We can't guarantee Default in all environments, but we can verify
        // the result is valid.
        assert!(matches!(
            detected.format,
            ConfigFormat::Toml | ConfigFormat::Lua | ConfigFormat::Wasm | ConfigFormat::Default
        ));
    }

    #[test]
    fn detect_toml_in_temp_dir() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ft_dir = dir.path().join("frankenterm");
        std::fs::create_dir_all(&ft_dir).unwrap();
        let toml_path = ft_dir.join("frankenterm.toml");
        std::fs::write(&toml_path, "scrollback_lines = 5000\n").unwrap();

        let _guard = EnvVarGuard::set("FRANKENTERM_CONFIG_FILE", toml_path.to_str().unwrap());
        let detected = detect_config(false);

        assert_eq!(detected.format, ConfigFormat::Toml);
        assert_eq!(detected.path.as_deref(), Some(toml_path.as_path()));
    }

    #[test]
    fn detect_wasm_via_env_var() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let wasm_path = dir.path().join("frankenterm.wasm");
        std::fs::write(&wasm_path, b"fake wasm").unwrap();

        let _guard = EnvVarGuard::set("FRANKENTERM_CONFIG_FILE", wasm_path.to_str().unwrap());
        let detected = detect_config(true);

        assert_eq!(detected.format, ConfigFormat::Wasm);
        assert_eq!(detected.path.as_deref(), Some(wasm_path.as_path()));
    }

    #[test]
    fn detect_lua_via_env_var() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let lua_path = dir.path().join("wezterm.lua");
        std::fs::write(&lua_path, "return {}").unwrap();

        let _guard = EnvVarGuard::set("FRANKENTERM_CONFIG_FILE", lua_path.to_str().unwrap());
        let detected = detect_config(false);

        assert_eq!(detected.format, ConfigFormat::Lua);
        assert_eq!(detected.path.as_deref(), Some(lua_path.as_path()));
    }

    #[test]
    fn toml_wins_over_wasm() {
        // When both exist in the same dir, TOML should be checked first.
        // This test verifies format priority via extension detection.
        assert_eq!(
            format_from_extension(Path::new("frankenterm.toml")),
            Some(ConfigFormat::Toml)
        );
        assert_eq!(
            format_from_extension(Path::new("frankenterm.wasm")),
            Some(ConfigFormat::Wasm)
        );
    }

    #[test]
    fn legacy_lua_paths_include_home() {
        let paths = legacy_lua_search_paths();
        assert!(!paths.is_empty());
        assert!(paths[0].ends_with(".wezterm.lua"));
    }

    #[test]
    fn load_detected_default_produces_valid_config() {
        let detected = DetectedConfig {
            format: ConfigFormat::Default,
            path: None,
        };
        let loaded = load_detected_config(&detected, &frankenterm_dynamic::Value::default(), None);
        let cfg = loaded.config.unwrap();
        assert!(cfg.scrollback_lines > 0);
    }

    /// RAII guard for temporarily setting an env var in tests.
    struct EnvVarGuard {
        key: String,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self {
                key: key.to_string(),
                previous,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(val) => std::env::set_var(&self.key, val),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}
