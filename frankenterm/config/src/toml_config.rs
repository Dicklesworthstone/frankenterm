//! TOML configuration loader for FrankenTerm.
//!
//! Loads a `frankenterm.toml` file and converts it to a [`Config`] struct via
//! the existing `toml_to_dynamic()` → `Config::from_dynamic()` pipeline.
//! This path does not require Lua and is always available.
//!
//! # Search order
//!
//! 1. Explicit `--config-file` override (if it points to TOML)
//! 2. `$FRANKENTERM_CONFIG_FILE` (environment variable)
//! 3. `~/.config/frankenterm/frankenterm.toml`
//! 4. `~/.frankenterm.toml`
//!
//! If no TOML config is found, returns `None` and the caller falls through
//! to Lua config or defaults.

use crate::{CONFIG_DIRS, CONFIG_FILE_OVERRIDE, HOME_DIR, LoadedConfig, toml_to_dynamic};
use anyhow::{Context, anyhow};
use frankenterm_dynamic::{FromDynamic, FromDynamicOptions, UnknownFieldAction, Value};
use std::path::{Path, PathBuf};

use super::Config;

/// Search for a `frankenterm.toml` file and load it if found.
///
/// Returns `Some(LoadedConfig)` if a TOML config was found and loaded
/// (successfully or with an error), or `None` if no TOML config exists.
pub(crate) fn try_load_toml_config(overrides: &Value) -> Option<LoadedConfig> {
    if let Some(override_path) = explicit_config_file_override_path() {
        if is_toml_path(&override_path) {
            return Some(load_required_toml_path(override_path, overrides));
        }
        // A non-TOML explicit override should be handled by the Lua/other path.
        return None;
    }

    if let Some(explicit_path) = explicit_toml_config_path_from_env() {
        return Some(load_required_toml_path(explicit_path, overrides));
    }

    let paths = toml_config_search_paths();

    for path in &paths {
        log::trace!("consider toml config: {}", path.display());
        match try_load_toml_file(path, overrides) {
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

fn is_toml_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("toml"))
}

fn explicit_config_file_override_path() -> Option<PathBuf> {
    CONFIG_FILE_OVERRIDE.lock().unwrap().clone()
}

fn explicit_toml_config_path_from_env() -> Option<PathBuf> {
    std::env::var_os("FRANKENTERM_CONFIG_FILE")
        .map(PathBuf::from)
        .filter(|path| is_toml_path(path))
}

fn load_required_toml_path(explicit_path: PathBuf, overrides: &Value) -> LoadedConfig {
    match try_load_toml_file(&explicit_path, overrides) {
        Ok(Some(loaded)) => loaded,
        Ok(None) => LoadedConfig {
            config: Err(anyhow!(
                "explicit TOML config path {} does not exist",
                explicit_path.display()
            )),
            file_name: Some(explicit_path),
            lua: None,
            warnings: vec![],
        },
        Err(err) => LoadedConfig {
            config: Err(err),
            file_name: Some(explicit_path),
            lua: None,
            warnings: vec![],
        },
    }
}

/// Build the list of paths to search for `frankenterm.toml`.
fn toml_config_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // XDG config dirs (reuse existing CONFIG_DIRS but for frankenterm subdir)
    for dir in CONFIG_DIRS.iter() {
        // CONFIG_DIRS points to wezterm subdirs; go up one level and add
        // frankenterm subdir.
        if let Some(parent) = dir.parent() {
            paths.push(parent.join("frankenterm").join("frankenterm.toml"));
        }
    }

    // Home directory dotfile
    paths.push(HOME_DIR.join(".frankenterm.toml"));

    paths
}

/// Try to load a single TOML config file.
///
/// Returns `Ok(None)` if the file does not exist, `Ok(Some(loaded))` on
/// success, or `Err` on parse/conversion failure.
fn try_load_toml_file(path: &Path, overrides: &Value) -> anyhow::Result<Option<LoadedConfig>> {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => anyhow::bail!("Error reading {}: {}", path.display(), err),
    };

    let (config, warnings) =
        frankenterm_dynamic::Error::capture_warnings(|| -> anyhow::Result<Config> {
            let toml_value: toml::Value = content
                .parse()
                .with_context(|| format!("Error parsing TOML from {}", path.display()))?;

            let mut dynamic = toml_to_dynamic(&toml_value);

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
                    "Error converting TOML config from {} to Config struct",
                    path.display()
                )
            })?;

            cfg.check_consistency()
                .context("check_consistency on TOML config")?;

            // Compute but discard the key bindings here so that we raise any
            // problems earlier than we use them.
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
    use std::ffi::OsString;
    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(prev) = &self.previous {
                std::env::set_var(self.key, prev);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    struct ConfigFileOverrideGuard {
        previous: Option<PathBuf>,
    }

    impl ConfigFileOverrideGuard {
        fn set(path: Option<&Path>) -> Self {
            let mut override_lock = crate::CONFIG_FILE_OVERRIDE.lock().unwrap();
            let previous = override_lock.clone();
            *override_lock = path.map(Path::to_path_buf);
            Self { previous }
        }
    }

    impl Drop for ConfigFileOverrideGuard {
        fn drop(&mut self) {
            *crate::CONFIG_FILE_OVERRIDE.lock().unwrap() = self.previous.clone();
        }
    }

    #[test]
    fn empty_toml_produces_default_config() {
        let toml_str = "";
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        // An empty TOML should produce a valid config with defaults
        assert!(cfg.scrollback_lines > 0);
    }

    #[test]
    fn basic_toml_fields_parse() {
        let toml_str = r#"
scrollback_lines = 10000
scrollback_tiered_enabled = true
scrollback_hot_lines = 1200
scrollback_warm_max_mb = 48
font_size = 14.0
color_scheme = "Catppuccin Mocha"
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        assert_eq!(cfg.scrollback_lines, 10000);
        assert!(cfg.scrollback_tiered_enabled);
        assert_eq!(cfg.scrollback_hot_lines, 1200);
        assert_eq!(cfg.scrollback_warm_max_mb, 48);
        assert!((cfg.font_size - 14.0).abs() < 0.01);
        assert_eq!(cfg.color_scheme.as_deref(), Some("Catppuccin Mocha"));
    }

    #[test]
    fn agent_detection_config_from_toml() {
        let toml_str = r#"
agent_detection_enabled = true
agent_active_threshold_ms = 3000
agent_thinking_threshold_ms = 8000
agent_stuck_threshold_ms = 45000
agent_idle_threshold_ms = 90000
agent_show_name_overlay = false
agent_show_backpressure = true
agent_border_width = 3
agent_auto_layout = "by_activity"
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        assert!(cfg.agent_detection_enabled);
        assert_eq!(cfg.agent_active_threshold_ms, 3000);
        assert_eq!(cfg.agent_thinking_threshold_ms, 8000);
        assert_eq!(cfg.agent_stuck_threshold_ms, 45000);
        assert_eq!(cfg.agent_idle_threshold_ms, 90000);
        assert!(!cfg.agent_show_name_overlay);
        assert!(cfg.agent_show_backpressure);
        assert_eq!(cfg.agent_border_width, 3);
        assert_eq!(cfg.agent_auto_layout, "by_activity");
    }

    #[test]
    fn toml_search_paths_returns_nonempty() {
        let paths = toml_config_search_paths();
        // Should always have at least the home dir dotfile
        assert!(!paths.is_empty());
        assert!(paths.last().unwrap().ends_with(".frankenterm.toml"));
    }

    #[test]
    fn nonexistent_toml_returns_none() {
        let result = try_load_toml_file(
            Path::new("/tmp/nonexistent_frankenterm_test_12345.toml"),
            &Value::default(),
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn malformed_toml_returns_error() {
        let path = std::env::temp_dir().join("frankenterm_test_malformed.toml");
        std::fs::write(&path, "this is [not valid {toml").unwrap();
        let result = try_load_toml_file(&path, &Value::default());
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err());
    }

    #[test]
    fn explicit_env_missing_toml_returns_error() {
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join("missing-config.toml");
        let _guard = EnvVarGuard::set("FRANKENTERM_CONFIG_FILE", &missing_path);

        let loaded = try_load_toml_config(&Value::default()).expect("expected explicit env result");
        assert_eq!(loaded.file_name, Some(missing_path));
        assert!(loaded.config.is_err());
    }

    #[test]
    fn explicit_env_toml_extension_is_case_insensitive() {
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frankenterm.TOML");
        std::fs::write(&path, "scrollback_lines = 4242\n").unwrap();
        let _guard = EnvVarGuard::set("FRANKENTERM_CONFIG_FILE", &path);

        let loaded = try_load_toml_config(&Value::default()).expect("expected explicit env result");
        assert_eq!(loaded.file_name, Some(path));
        let cfg = loaded.config.expect("expected parsed config");
        assert_eq!(cfg.scrollback_lines, 4242);
    }

    #[test]
    fn explicit_override_missing_toml_returns_error() {
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _env_guard = EnvVarGuard::set(
            "FRANKENTERM_CONFIG_FILE",
            Path::new("/tmp/this_should_not_be_considered.toml"),
        );

        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join("missing-override.toml");
        let _override_guard = ConfigFileOverrideGuard::set(Some(&missing_path));

        let loaded = try_load_toml_config(&Value::default()).expect("expected override result");
        assert_eq!(loaded.file_name, Some(missing_path));
        assert!(loaded.config.is_err());
    }

    #[test]
    fn non_toml_override_skips_toml_loader_even_with_env_toml() {
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let env_toml = dir.path().join("from-env.toml");
        std::fs::write(&env_toml, "scrollback_lines = 1111\n").unwrap();
        let _env_guard = EnvVarGuard::set("FRANKENTERM_CONFIG_FILE", &env_toml);

        let override_lua = dir.path().join("override.lua");
        let _override_guard = ConfigFileOverrideGuard::set(Some(&override_lua));

        let loaded = try_load_toml_config(&Value::default());
        assert!(
            loaded.is_none(),
            "non-TOML override should bypass TOML loader"
        );
    }

    #[test]
    fn full_config_loads_all_fields() {
        let toml_str = r#"
scrollback_lines = 50000
font_size = 16.0
color_scheme = "Builtin Dark"
enable_scroll_bar = true
enable_tab_bar = true
hide_tab_bar_if_only_one_tab = true
tab_bar_at_bottom = false
initial_rows = 40
initial_cols = 120
window_close_confirmation = "NeverPrompt"
check_for_updates = false
automatically_reload_config = true
max_fps = 60

[window_padding]
left = 4
right = 4
top = 4
bottom = 4

[[font.font]]
family = "JetBrains Mono"
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        assert_eq!(cfg.scrollback_lines, 50000);
        assert!((cfg.font_size - 16.0).abs() < 0.01);
        assert_eq!(cfg.color_scheme.as_deref(), Some("Builtin Dark"));
        assert_eq!(cfg.enable_scroll_bar, true);
        assert_eq!(cfg.initial_rows, 40);
        assert_eq!(cfg.initial_cols, 120);
    }

    #[test]
    fn type_mismatch_produces_error() {
        // scrollback_lines should be integer, not string
        let toml_str = r#"scrollback_lines = "not a number""#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let result = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn default_values_for_unpopulated_keys() {
        let toml_str = "color_scheme = \"Test\"\n";
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        // scrollback_lines should have a default > 0
        assert!(cfg.scrollback_lines > 0);
        // font_size should have a reasonable default
        assert!(cfg.font_size > 0.0);
        // initial_rows/cols should have defaults
        assert!(cfg.initial_rows > 0);
        assert!(cfg.initial_cols > 0);
    }

    #[test]
    fn window_padding_config_loads() {
        let toml_str = r#"
[window_padding]
left = 8
right = 8
top = 12
bottom = 12
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        );
        assert!(
            cfg.is_ok(),
            "window_padding config should parse without error"
        );
    }

    #[test]
    fn overrides_take_precedence() {
        let toml_str = r#"
scrollback_lines = 5000
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let mut dynamic = toml_to_dynamic(&toml_value);

        // Override scrollback_lines to 9999
        let mut ovr_map = std::collections::BTreeMap::new();
        ovr_map.insert(Value::String("scrollback_lines".into()), Value::U64(9999));
        let override_val = Value::Object(ovr_map.into());

        if let (Value::Object(ref mut base), Value::Object(ref ovr)) = (&mut dynamic, &override_val)
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
        .unwrap();
        assert_eq!(cfg.scrollback_lines, 9999);
    }

    #[test]
    fn resize_wrap_scorecard_config_loads() {
        // Use non-default values to prove TOML parsing overrides defaults
        // (defaults are: 500, 2000, 20)
        let toml_str = r#"
resize_wrap_scorecard_enabled = true
resize_wrap_readability_gate_enabled = true
resize_wrap_readability_max_line_badness_delta = 1000
resize_wrap_readability_max_total_badness_delta = 5000
resize_wrap_readability_max_fallback_ratio_percent = 40
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        assert!(cfg.resize_wrap_scorecard_enabled);
        assert!(cfg.resize_wrap_readability_gate_enabled);
        assert_eq!(cfg.resize_wrap_readability_max_line_badness_delta, 1000);
        assert_eq!(cfg.resize_wrap_readability_max_total_badness_delta, 5000);
        assert_eq!(cfg.resize_wrap_readability_max_fallback_ratio_percent, 40);
    }

    #[test]
    fn resize_wrap_kp_cost_model_config_loads() {
        let toml_str = r#"
resize_wrap_kp_badness_scale = 20000
resize_wrap_kp_forced_break_penalty = 10000
resize_wrap_kp_lookahead_limit = 128
resize_wrap_kp_max_dp_states = 16384
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        assert_eq!(cfg.resize_wrap_kp_badness_scale, 20000);
        assert_eq!(cfg.resize_wrap_kp_forced_break_penalty, 10000);
        assert_eq!(cfg.resize_wrap_kp_lookahead_limit, 128);
        assert_eq!(cfg.resize_wrap_kp_max_dp_states, 16384);
    }

    #[test]
    fn resize_wrap_defaults_when_not_specified() {
        let toml_str = r#"
scrollback_lines = 5000
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        // Defaults: scorecard and gate are enabled for resize quality telemetry
        assert!(cfg.resize_wrap_scorecard_enabled);
        assert!(cfg.resize_wrap_readability_gate_enabled);
        // Default KP cost model values
        assert_eq!(cfg.resize_wrap_kp_badness_scale, 10_000);
        assert_eq!(cfg.resize_wrap_kp_forced_break_penalty, 5_000);
        assert_eq!(cfg.resize_wrap_kp_lookahead_limit, 64);
        assert_eq!(cfg.resize_wrap_kp_max_dp_states, 8_192);
        // Default gate thresholds (sensible production values)
        assert_eq!(cfg.resize_wrap_readability_max_line_badness_delta, 500);
        assert_eq!(cfg.resize_wrap_readability_max_total_badness_delta, 2000);
        assert_eq!(cfg.resize_wrap_readability_max_fallback_ratio_percent, 20);
    }

    #[test]
    fn resize_wrap_fallback_ratio_percent_clamped() {
        // Values above 100 should still parse (validation happens separately)
        let toml_str = r#"
resize_wrap_readability_max_fallback_ratio_percent = 50
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        assert_eq!(cfg.resize_wrap_readability_max_fallback_ratio_percent, 50);
    }

    #[test]
    fn resize_wrap_scorecard_can_be_explicitly_disabled() {
        let toml_str = r#"
resize_wrap_scorecard_enabled = false
resize_wrap_readability_gate_enabled = false
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        assert!(!cfg.resize_wrap_scorecard_enabled);
        assert!(!cfg.resize_wrap_readability_gate_enabled);
    }

    #[test]
    fn ssh_domain_config_parses() {
        let toml_str = r#"
[[ssh_domains]]
name = "production"
remote_address = "10.0.0.5:22"
username = "deploy"
connect_automatically = true

[[ssh_domains]]
name = "staging"
remote_address = "staging.example.com"
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        let domains = cfg.ssh_domains.expect("ssh_domains should be Some");
        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0].name, "production");
        assert_eq!(domains[0].remote_address, "10.0.0.5:22");
        assert_eq!(domains[0].username.as_deref(), Some("deploy"));
        // connect_automatically defaults to false; verify TOML override to true
        assert!(domains[0].connect_automatically);
        assert_eq!(domains[1].name, "staging");
        assert_eq!(domains[1].remote_address, "staging.example.com");
        assert!(domains[1].username.is_none());
        // staging doesn't set connect_automatically, so it should default to false
        assert!(!domains[1].connect_automatically);
    }

    #[test]
    fn ssh_domain_defaults_from_ssh_config() {
        // When no ssh_domains specified, ssh_domains() auto-discovers from ~/.ssh/config
        let toml_str = r#"
scrollback_lines = 5000
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        // When not explicitly set, ssh_domains is None (auto-discovered at runtime)
        assert!(cfg.ssh_domains.is_none());
        // The ssh_domains() method will auto-discover; just verify the field is unset
    }

    #[test]
    fn ssh_domain_with_options() {
        let toml_str = r#"
[[ssh_domains]]
name = "custom"
remote_address = "myhost.local"
multiplexing = "None"
no_agent_auth = true
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        let domains = cfg.ssh_domains.expect("ssh_domains should be Some");
        assert_eq!(domains.len(), 1);
        assert_eq!(domains[0].name, "custom");
        assert!(domains[0].no_agent_auth);
        assert_eq!(domains[0].multiplexing, crate::SshMultiplexing::None);
    }
}
