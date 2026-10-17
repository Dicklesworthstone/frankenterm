//! Performance-hostile configuration advisories (ft-yccm0.2.7).
//!
//! Some settings cost throughput or responsiveness without saying so: an
//! oversized parser buffer, an fps cap under the display's refresh rate, huge
//! untiered scrollback, a CPU front end, and a GUI older than the `ft` that
//! drives it. The GUI logs these at startup and `ft doctor` reports them, both
//! under the stable codes below. `docs/tuning-reference.md` has one entry per
//! code explaining the tradeoff.
//!
//! `ft doctor` cannot evaluate the Lua config or see the display, so a running
//! GUI publishes the facts it judged itself by ([`GuiTuningFacts`]); without
//! one, doctor scans the config file as text ([`scan_config_text`]).

use crate::{Config, FrontEndSelection};
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use std::path::{Path, PathBuf};

/// The `mux_output_parser_buffer_size` default; larger buffers are reported.
pub const PARSER_BUFFER_ADVISORY_BYTES: usize = 128 * 1024;

/// Untiered scrollback longer than this many lines per pane is reported.
pub const UNTIERED_SCROLLBACK_ADVISORY_LINES: usize = 10_000;

/// The refresh rate `max_fps` is held to when no display reported one: the
/// `max_fps` default.
pub const ASSUMED_DISPLAY_REFRESH_HZ: u64 = 60;

/// File-name prefix of a GUI's published [`GuiTuningFacts`].
pub const GUI_TUNING_FILE_PREFIX: &str = "gui-tuning-";

/// A stable advisory code. Codes are never renumbered or reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TuningCode {
    /// `FT-TUNE-0001`: `mux_output_parser_buffer_size` above the default.
    ParserBufferOversized,
    /// `FT-TUNE-0002`: `max_fps` below the display's refresh rate.
    FpsCapBelowRefresh,
    /// `FT-TUNE-0003`: very large scrollback with tiering disabled.
    LargeUntieredScrollback,
    /// `FT-TUNE-0004`: a front end that renders on the CPU.
    SlowFrontEnd,
    /// `FT-TUNE-0005`: the GUI is an older release than `ft`.
    GuiOlderThanCli,
}

impl TuningCode {
    pub const ALL: [Self; 5] = [
        Self::ParserBufferOversized,
        Self::FpsCapBelowRefresh,
        Self::LargeUntieredScrollback,
        Self::SlowFrontEnd,
        Self::GuiOlderThanCli,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ParserBufferOversized => "FT-TUNE-0001",
            Self::FpsCapBelowRefresh => "FT-TUNE-0002",
            Self::LargeUntieredScrollback => "FT-TUNE-0003",
            Self::SlowFrontEnd => "FT-TUNE-0004",
            Self::GuiOlderThanCli => "FT-TUNE-0005",
        }
    }

    /// The setting, or installation fact, the advisory is about.
    pub fn subject(self) -> &'static str {
        match self {
            Self::ParserBufferOversized => "mux_output_parser_buffer_size",
            Self::FpsCapBelowRefresh => "max_fps",
            Self::LargeUntieredScrollback => "scrollback_lines",
            Self::SlowFrontEnd => "front_end",
            Self::GuiOlderThanCli => "gui_version",
        }
    }

    /// Where `docs/tuning-reference.md` explains this code.
    pub fn docs(self) -> &'static str {
        match self {
            Self::ParserBufferOversized => "docs/tuning-reference.md#ft-tune-0001",
            Self::FpsCapBelowRefresh => "docs/tuning-reference.md#ft-tune-0002",
            Self::LargeUntieredScrollback => "docs/tuning-reference.md#ft-tune-0003",
            Self::SlowFrontEnd => "docs/tuning-reference.md#ft-tune-0004",
            Self::GuiOlderThanCli => "docs/tuning-reference.md#ft-tune-0005",
        }
    }
}

impl Serialize for TuningCode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// A display and its refresh rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayRefresh {
    pub name: String,
    pub hz: u64,
}

/// A GUI build, judged against the `ft` that judges it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuiGeneration {
    /// Where the version came from: a running GUI or an app bundle.
    pub source: String,
    pub version: String,
}

/// Everything the advisories read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuningInputs {
    pub mux_output_parser_buffer_size: usize,
    pub max_fps: u64,
    pub scrollback_lines: usize,
    pub scrollback_tiered_enabled: bool,
    pub scrollback_hot_lines: usize,
    pub front_end: FrontEndSelection,
    pub webgpu_force_fallback_adapter: bool,
    /// The fastest display, when one reported its refresh rate.
    pub display: Option<DisplayRefresh>,
    /// The GUI to compare with `cli_version`.
    pub gui: Option<GuiGeneration>,
    /// The judging `ft`'s version.
    pub cli_version: Option<String>,
}

impl TuningInputs {
    /// The settings of `config`, with no display or version facts.
    pub fn from_config(config: &Config) -> Self {
        Self {
            mux_output_parser_buffer_size: config.mux_output_parser_buffer_size,
            max_fps: config.max_fps,
            scrollback_lines: config.scrollback_lines,
            scrollback_tiered_enabled: config.scrollback_tiered_enabled,
            scrollback_hot_lines: config.scrollback_hot_lines,
            front_end: config.front_end,
            webgpu_force_fallback_adapter: config.webgpu_force_fallback_adapter,
            display: None,
            gui: None,
            cli_version: None,
        }
    }
}

/// One performance-hostile setting, what it costs, and what to change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TuningAdvisory {
    pub code: TuningCode,
    pub subject: &'static str,
    pub detail: String,
    pub remediation: String,
    pub docs: &'static str,
}

impl TuningAdvisory {
    fn new(code: TuningCode, detail: String, remediation: String) -> Self {
        Self {
            code,
            subject: code.subject(),
            detail,
            remediation,
            docs: code.docs(),
        }
    }

    /// The GUI startup log line.
    pub fn log_line(&self) -> String {
        format!(
            "{} {}: {} Fix: {} ({})",
            self.code.as_str(),
            self.subject,
            self.detail,
            self.remediation,
            self.docs
        )
    }
}

/// Every advisory `inputs` trips, in code order.
pub fn tuning_advisories(inputs: &TuningInputs) -> Vec<TuningAdvisory> {
    let mut advisories = Vec::new();

    if inputs.mux_output_parser_buffer_size > PARSER_BUFFER_ADVISORY_BYTES {
        advisories.push(TuningAdvisory::new(
            TuningCode::ParserBufferOversized,
            format!(
                "mux_output_parser_buffer_size is {}; the default is {}. Each pty read takes \
                 up to that many bytes and the parsed batch is applied under the pane's \
                 terminal lock, so paint, mouse and resize wait behind longer lock holds, and \
                 output that backed up during a stall arrives in bursts of that size.",
                format_bytes(inputs.mux_output_parser_buffer_size),
                format_bytes(PARSER_BUFFER_ADVISORY_BYTES),
            ),
            format!(
                "Remove mux_output_parser_buffer_size from the config, or set it to {} or less.",
                PARSER_BUFFER_ADVISORY_BYTES
            ),
        ));
    }

    let (refresh_hz, refresh) = match &inputs.display {
        Some(display) => (
            display.hz,
            format!(
                "the {} Hz refresh rate of display {:?}",
                display.hz, display.name
            ),
        ),
        None => (
            ASSUMED_DISPLAY_REFRESH_HZ,
            format!(
                "{} Hz (no running GUI reported its display; {} is the max_fps default)",
                ASSUMED_DISPLAY_REFRESH_HZ, ASSUMED_DISPLAY_REFRESH_HZ
            ),
        ),
    };
    if inputs.max_fps < refresh_hz {
        advisories.push(TuningAdvisory::new(
            TuningCode::FpsCapBelowRefresh,
            format!(
                "max_fps = {} caps repaints below {}, so output and typing echo reach the \
                 screen at most {} times a second.",
                inputs.max_fps, refresh, inputs.max_fps
            ),
            format!("Set max_fps = {}.", refresh_hz),
        ));
    }

    if !inputs.scrollback_tiered_enabled
        && inputs.scrollback_lines > UNTIERED_SCROLLBACK_ADVISORY_LINES
    {
        advisories.push(TuningAdvisory::new(
            TuningCode::LargeUntieredScrollback,
            format!(
                "scrollback_lines = {} with scrollback_tiered_enabled = false keeps every \
                 scrollback line of every pane resident in memory, and a resize rewraps all \
                 of them.",
                inputs.scrollback_lines
            ),
            format!(
                "Set scrollback_tiered_enabled = true (the default), which keeps the newest \
                 scrollback_hot_lines ({}) lines hot and moves older lines to the warm and cold \
                 tiers; or set scrollback_lines to {} or less.",
                inputs.scrollback_hot_lines, UNTIERED_SCROLLBACK_ADVISORY_LINES
            ),
        ));
    }

    if let Some((detail, remediation)) =
        slow_front_end(inputs.front_end, inputs.webgpu_force_fallback_adapter)
    {
        advisories.push(TuningAdvisory::new(
            TuningCode::SlowFrontEnd,
            detail.to_string(),
            remediation.to_string(),
        ));
    }

    if let Some(advisory) = gui_generation_advisory(inputs) {
        advisories.push(advisory);
    }

    advisories
}

/// Why `front_end` renders on the CPU on every platform, and the fix.
fn slow_front_end(
    front_end: FrontEndSelection,
    force_fallback_adapter: bool,
) -> Option<(&'static str, &'static str)> {
    match front_end {
        FrontEndSelection::Software => Some((
            "front_end = \"Software\" draws every frame with a software (CPU) OpenGL renderer \
             instead of the GPU.",
            "Remove front_end to use the default GPU renderer (OpenGL), or set \
             front_end = \"WebGpu\".",
        )),
        FrontEndSelection::WebGpu if force_fallback_adapter => Some((
            "front_end = \"WebGpu\" with webgpu_force_fallback_adapter = true makes wgpu use \
             its fallback adapter, which is generally a software (CPU) implementation.",
            "Set webgpu_force_fallback_adapter = false (the default).",
        )),
        _ => None,
    }
}

fn gui_generation_advisory(inputs: &TuningInputs) -> Option<TuningAdvisory> {
    let gui = inputs.gui.as_ref()?;
    let gui_version = release_version(&gui.version)?;
    let cli_version = release_version(inputs.cli_version.as_deref()?)?;
    if gui_version >= cli_version {
        return None;
    }
    Some(TuningAdvisory::new(
        TuningCode::GuiOlderThanCli,
        format!(
            "{} is {}, older than this ft ({}): the GUI runs without every change released \
             after {}.",
            gui.source, gui_version, cli_version, gui_version
        ),
        format!(
            "Install FrankenTerm.app from this ft's release (install.sh --version {}) and \
             restart FrankenTerm.",
            cli_version
        ),
    ))
}

/// The first semantic version in `text`, as in `0.15.21`, `v0.15.21`,
/// `FrankenTerm 0.15.21 (<commit>)` or `ft 0.15.6-rc.53 (<commit>)`.
pub fn release_version(text: &str) -> Option<semver::Version> {
    text.split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .find_map(|token| semver::Version::parse(token.strip_prefix('v').unwrap_or(token)).ok())
}

fn format_bytes(bytes: usize) -> String {
    if bytes.is_multiple_of(1024) {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{} bytes", bytes)
    }
}

/// The config spelling of `front_end`.
pub fn front_end_name(front_end: FrontEndSelection) -> &'static str {
    match front_end {
        FrontEndSelection::OpenGL => "OpenGL",
        FrontEndSelection::WebGpu => "WebGpu",
        FrontEndSelection::Software => "Software",
        FrontEndSelection::Metal => "Metal",
    }
}

/// The front end a config spells `name`.
pub fn front_end_from_name(name: &str) -> Option<FrontEndSelection> {
    [
        FrontEndSelection::OpenGL,
        FrontEndSelection::WebGpu,
        FrontEndSelection::Software,
        FrontEndSelection::Metal,
    ]
    .iter()
    .copied()
    .find(|front_end| front_end_name(*front_end) == name)
}

/// The config files the GUI would load, first existing one wins, in
/// `Config::load_with_overrides` order: the TOML search (a `.toml`
/// `FRANKENTERM_CONFIG_FILE` first), then the Lua search (an explicit
/// `FRANKENTERM_CONFIG_FILE` or `WEZTERM_CONFIG_FILE` first). Lua files are
/// listed whether or not `FRANKENTERM_LUA_CONFIG` enables them in this
/// environment: the GUI may run with a different one.
pub fn gui_config_candidates() -> Vec<PathBuf> {
    let explicit = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let frankenterm_file = explicit("FRANKENTERM_CONFIG_FILE");
    let is_toml = |path: &PathBuf| {
        path.extension()
            .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("toml"))
    };
    let mut paths: Vec<PathBuf> = frankenterm_file
        .iter()
        .filter(|path| is_toml(*path))
        .cloned()
        .collect();
    paths.extend(
        crate::frankenterm_config_dirs()
            .into_iter()
            .map(|dir| dir.join("frankenterm.toml")),
    );
    paths.push(crate::HOME_DIR.join(".frankenterm.toml"));
    paths.extend(frankenterm_file.into_iter().filter(|path| !is_toml(path)));
    paths.extend(explicit("WEZTERM_CONFIG_FILE"));
    paths.push(crate::HOME_DIR.join(".frankenterm.lua"));
    paths.extend(
        crate::CONFIG_DIRS
            .iter()
            .map(|dir| dir.join("frankenterm.lua")),
    );
    paths.push(crate::HOME_DIR.join(".wezterm.lua"));
    paths.extend(crate::CONFIG_DIRS.iter().map(|dir| dir.join("wezterm.lua")));
    paths
}

/// The advisory settings a config file assigns, read as text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScannedTuningSettings {
    pub mux_output_parser_buffer_size: Option<usize>,
    pub max_fps: Option<u64>,
    pub scrollback_lines: Option<usize>,
    pub scrollback_tiered_enabled: Option<bool>,
    pub scrollback_hot_lines: Option<usize>,
    pub front_end: Option<FrontEndSelection>,
    pub webgpu_force_fallback_adapter: Option<bool>,
}

impl ScannedTuningSettings {
    /// Overlay the scanned settings on `inputs`.
    pub fn apply_to(&self, inputs: &mut TuningInputs) {
        if let Some(value) = self.mux_output_parser_buffer_size {
            inputs.mux_output_parser_buffer_size = value;
        }
        if let Some(value) = self.max_fps {
            inputs.max_fps = value;
        }
        if let Some(value) = self.scrollback_lines {
            inputs.scrollback_lines = value;
        }
        if let Some(value) = self.scrollback_tiered_enabled {
            inputs.scrollback_tiered_enabled = value;
        }
        if let Some(value) = self.scrollback_hot_lines {
            inputs.scrollback_hot_lines = value;
        }
        if let Some(value) = self.front_end {
            inputs.front_end = value;
        }
        if let Some(value) = self.webgpu_force_fallback_adapter {
            inputs.webgpu_force_fallback_adapter = value;
        }
    }

    /// The settings that were found.
    pub fn found(&self) -> Vec<&'static str> {
        [
            (
                "mux_output_parser_buffer_size",
                self.mux_output_parser_buffer_size.is_some(),
            ),
            ("max_fps", self.max_fps.is_some()),
            ("scrollback_lines", self.scrollback_lines.is_some()),
            (
                "scrollback_tiered_enabled",
                self.scrollback_tiered_enabled.is_some(),
            ),
            ("scrollback_hot_lines", self.scrollback_hot_lines.is_some()),
            ("front_end", self.front_end.is_some()),
            (
                "webgpu_force_fallback_adapter",
                self.webgpu_force_fallback_adapter.is_some(),
            ),
        ]
        .iter()
        .filter(|(_, found)| *found)
        .map(|(name, _)| *name)
        .collect()
    }
}

/// Scan a `frankenterm.lua`, `wezterm.lua` or `frankenterm.toml` for the
/// advisory settings without executing it. An assignment is `name = value`
/// where `name` may be qualified (`config.max_fps`), as a Lua statement, a
/// table field or a TOML key. Integers may be products (`512 * 1024`) and may
/// use `_` separators. `--`, `--[[ ]]` and `#` comments are skipped. The last
/// assignment wins, as when Lua runs top to bottom. Nothing is evaluated: an
/// assignment on its own line inside a conditional block counts as made, and
/// one sharing its line with the `if` is not read.
pub fn scan_config_text(text: &str) -> ScannedTuningSettings {
    let mut scanned = ScannedTuningSettings::default();
    let mut in_block_comment = false;
    for line in text.lines() {
        let mut code = line;
        if in_block_comment {
            match code.find("]]") {
                Some(end) => {
                    in_block_comment = false;
                    code = &code[end + 2..];
                }
                None => continue,
            }
        }
        if let Some(start) = code.find("--[[") {
            in_block_comment = !code[start + 4..].contains("]]");
            code = &code[..start];
        }
        let code = code.split("--").next().unwrap_or("");
        let code = code.split('#').next().unwrap_or("");
        let Some((name, value)) = code.split_once('=') else {
            continue;
        };
        if value.starts_with('=') {
            continue;
        }
        let name = name.trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        {
            continue;
        }
        let key = name.rsplit('.').next().unwrap_or(name);
        let value = value.trim().trim_end_matches([',', ';']).trim();
        match key {
            "mux_output_parser_buffer_size" => keep(
                &mut scanned.mux_output_parser_buffer_size,
                scan_integer(value).and_then(|value| usize::try_from(value).ok()),
            ),
            "max_fps" => keep(&mut scanned.max_fps, scan_integer(value)),
            "scrollback_lines" => keep(
                &mut scanned.scrollback_lines,
                scan_integer(value).and_then(|value| usize::try_from(value).ok()),
            ),
            "scrollback_tiered_enabled" => {
                keep(&mut scanned.scrollback_tiered_enabled, scan_bool(value))
            }
            "scrollback_hot_lines" => keep(
                &mut scanned.scrollback_hot_lines,
                scan_integer(value).and_then(|value| usize::try_from(value).ok()),
            ),
            "front_end" => keep(
                &mut scanned.front_end,
                scan_string(value).and_then(front_end_from_name),
            ),
            "webgpu_force_fallback_adapter" => {
                keep(&mut scanned.webgpu_force_fallback_adapter, scan_bool(value))
            }
            _ => {}
        }
    }
    scanned
}

fn keep<T>(slot: &mut Option<T>, value: Option<T>) {
    if value.is_some() {
        *slot = value;
    }
}

fn scan_integer(value: &str) -> Option<u64> {
    let value = value
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
        .unwrap_or(value);
    value.split('*').try_fold(1u64, |product, factor| {
        let digits: String = factor.trim().chars().filter(|c| *c != '_').collect();
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        product.checked_mul(digits.parse().ok()?)
    })
}

fn scan_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn scan_string(value: &str) -> Option<&str> {
    ['"', '\''].iter().find_map(|quote| {
        value
            .strip_prefix(*quote)
            .and_then(|inner| inner.strip_suffix(*quote))
    })
}

/// The settings and display a running GUI judged itself by, published for
/// `ft doctor` into the runtime directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuiTuningFacts {
    pub pid: u32,
    /// The GUI's version string, e.g. `FrankenTerm 0.15.21 (<commit>)`.
    pub version: String,
    pub mux_output_parser_buffer_size: usize,
    pub max_fps: u64,
    pub scrollback_lines: usize,
    pub scrollback_tiered_enabled: bool,
    pub scrollback_hot_lines: usize,
    /// The configured front end, as the config spells it.
    pub front_end: String,
    pub webgpu_force_fallback_adapter: bool,
    /// The fastest display when the GUI started, if any reported its rate.
    pub display: Option<DisplayRefresh>,
}

impl GuiTuningFacts {
    /// Facts for process `pid`, running `version`, judged by `inputs`.
    pub fn new(pid: u32, version: &str, inputs: &TuningInputs) -> Self {
        Self {
            pid,
            version: version.to_string(),
            mux_output_parser_buffer_size: inputs.mux_output_parser_buffer_size,
            max_fps: inputs.max_fps,
            scrollback_lines: inputs.scrollback_lines,
            scrollback_tiered_enabled: inputs.scrollback_tiered_enabled,
            scrollback_hot_lines: inputs.scrollback_hot_lines,
            front_end: front_end_name(inputs.front_end).to_string(),
            webgpu_force_fallback_adapter: inputs.webgpu_force_fallback_adapter,
            display: inputs.display.clone(),
        }
    }

    /// Advisory inputs for this GUI, judged by an `ft` at `cli_version`. An
    /// unknown front-end name (from a newer GUI) reads as the default.
    pub fn to_inputs(&self, cli_version: &str) -> TuningInputs {
        TuningInputs {
            mux_output_parser_buffer_size: self.mux_output_parser_buffer_size,
            max_fps: self.max_fps,
            scrollback_lines: self.scrollback_lines,
            scrollback_tiered_enabled: self.scrollback_tiered_enabled,
            scrollback_hot_lines: self.scrollback_hot_lines,
            front_end: front_end_from_name(&self.front_end).unwrap_or_default(),
            webgpu_force_fallback_adapter: self.webgpu_force_fallback_adapter,
            display: self.display.clone(),
            gui: Some(GuiGeneration {
                source: format!("the running GUI (pid {})", self.pid),
                version: self.version.clone(),
            }),
            cli_version: Some(cli_version.to_string()),
        }
    }
}

/// Path of the facts `pid` publishes into `dir`.
pub fn gui_tuning_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{}{}.json", GUI_TUNING_FILE_PREFIX, pid))
}

/// The facts `pid` published into `dir`, or `None` if it published none.
pub fn read_gui_tuning(dir: &Path, pid: u32) -> std::io::Result<Option<GuiTuningFacts>> {
    let bytes = match std::fs::read(gui_tuning_path(dir, pid)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// This process's published facts file. Dropping it removes the file.
#[derive(Debug)]
pub struct GuiTuningPublication {
    dir: PathBuf,
    path: PathBuf,
    pid: u32,
}

impl GuiTuningPublication {
    /// Publish `facts` into `dir`, creating `dir` if needed.
    pub fn publish(dir: &Path, facts: &GuiTuningFacts) -> std::io::Result<Self> {
        let publication = Self {
            dir: dir.to_path_buf(),
            path: gui_tuning_path(dir, facts.pid),
            pid: facts.pid,
        };
        publication.republish(facts)?;
        Ok(publication)
    }

    /// Replace the published facts atomically (write, then rename).
    pub fn republish(&self, facts: &GuiTuningFacts) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        // The leading dot keeps a half-written file out of a reader's path.
        let staging = self
            .dir
            .join(format!(".{}{}.json.tmp", GUI_TUNING_FILE_PREFIX, self.pid));
        let bytes = serde_json::to_vec_pretty(facts)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        std::fs::write(&staging, bytes)?;
        std::fs::rename(&staging, &self.path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for GuiTuningPublication {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("removing {}: {}", self.path.display(), error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_dynamic::Value;

    /// The advisory inputs of a `frankenterm.toml` fixture, through the real
    /// config parser.
    fn inputs_for(toml: &str) -> TuningInputs {
        let config = crate::parse_toml_config_from_str(toml, &Value::default())
            .unwrap_or_else(|error| panic!("fixture {:?} must parse: {:#}", toml, error));
        TuningInputs::from_config(&config)
    }

    fn codes(inputs: &TuningInputs) -> Vec<&'static str> {
        tuning_advisories(inputs)
            .iter()
            .map(|advisory| advisory.code.as_str())
            .collect()
    }

    fn display(hz: u64) -> Option<DisplayRefresh> {
        Some(DisplayRefresh {
            name: "Built-in Retina Display".to_string(),
            hz,
        })
    }

    #[test]
    fn codes_are_stable_and_thresholds_track_the_config_defaults() {
        let all: Vec<&str> = TuningCode::ALL.iter().map(|code| code.as_str()).collect();
        assert_eq!(
            all,
            [
                "FT-TUNE-0001",
                "FT-TUNE-0002",
                "FT-TUNE-0003",
                "FT-TUNE-0004",
                "FT-TUNE-0005"
            ]
        );
        for code in TuningCode::ALL.iter() {
            assert!(
                code.docs()
                    .ends_with(&format!("#{}", code.as_str().to_ascii_lowercase())),
                "{}",
                code.docs()
            );
            assert_eq!(
                serde_json::to_value(code).unwrap(),
                serde_json::json!(code.as_str())
            );
        }
        let defaults = TuningInputs::from_config(&Config::default_config());
        assert_eq!(
            defaults.mux_output_parser_buffer_size,
            PARSER_BUFFER_ADVISORY_BYTES
        );
        assert_eq!(defaults.max_fps, ASSUMED_DISPLAY_REFRESH_HZ);
        assert!(
            tuning_advisories(&defaults).is_empty(),
            "the default config is not performance-hostile: {:?}",
            tuning_advisories(&defaults)
        );
    }

    #[test]
    fn every_code_has_a_tuning_reference_entry() {
        let reference = include_str!("../../../docs/tuning-reference.md");
        for code in TuningCode::ALL.iter() {
            let heading = format!("\n### {}\n", code.as_str());
            assert!(
                reference.contains(&heading),
                "docs/tuning-reference.md has no {:?} entry",
                heading.trim()
            );
        }
    }

    #[test]
    fn parser_buffer_above_the_default_is_reported_with_its_size() {
        let oversized = inputs_for("mux_output_parser_buffer_size = 524288\n");
        let advisories = tuning_advisories(&oversized);
        assert_eq!(codes(&oversized), ["FT-TUNE-0001"]);
        let advisory = &advisories[0];
        assert_eq!(advisory.subject, "mux_output_parser_buffer_size");
        assert!(
            advisory.detail.contains("is 512 KiB"),
            "{}",
            advisory.detail
        );
        assert!(
            advisory.detail.contains("terminal lock"),
            "{}",
            advisory.detail
        );
        assert!(
            advisory.remediation.contains("131072 or less"),
            "{}",
            advisory.remediation
        );

        assert!(codes(&inputs_for("mux_output_parser_buffer_size = 131072\n")).is_empty());
        assert!(codes(&inputs_for("mux_output_parser_buffer_size = 4096\n")).is_empty());
    }

    #[test]
    fn fps_cap_below_the_display_refresh_is_reported() {
        let mut capped = inputs_for("max_fps = 30\n");
        // No GUI reported a display: held to the 60 fps default.
        assert_eq!(codes(&capped), ["FT-TUNE-0002"]);
        let advisory = &tuning_advisories(&capped)[0];
        assert!(
            advisory.detail.contains("max_fps = 30"),
            "{}",
            advisory.detail
        );
        assert!(
            advisory.detail.contains("no running GUI reported"),
            "{}",
            advisory.detail
        );
        assert_eq!(advisory.remediation, "Set max_fps = 60.");

        capped.display = display(120);
        let advisory = &tuning_advisories(&capped)[0];
        assert!(
            advisory.detail.contains("120 Hz refresh rate of display"),
            "{}",
            advisory.detail
        );
        assert_eq!(advisory.remediation, "Set max_fps = 120.");

        // The 60 fps default on a 120 Hz display is still a cap below it.
        let mut default_fps = inputs_for("");
        default_fps.display = display(120);
        assert_eq!(codes(&default_fps), ["FT-TUNE-0002"]);

        let mut matched = inputs_for("max_fps = 120\n");
        matched.display = display(120);
        assert!(codes(&matched).is_empty());
        let mut above = inputs_for("max_fps = 144\n");
        above.display = display(60);
        assert!(codes(&above).is_empty());
    }

    #[test]
    fn large_scrollback_is_reported_only_when_untiered() {
        let untiered = inputs_for("scrollback_lines = 100000\nscrollback_tiered_enabled = false\n");
        assert_eq!(codes(&untiered), ["FT-TUNE-0003"]);
        let advisory = &tuning_advisories(&untiered)[0];
        assert!(
            advisory.detail.contains("scrollback_lines = 100000"),
            "{}",
            advisory.detail
        );
        assert!(
            advisory.remediation.contains("scrollback_hot_lines (1000)"),
            "{}",
            advisory.remediation
        );

        assert!(codes(&inputs_for("scrollback_lines = 100000\n")).is_empty());
        assert!(codes(&inputs_for(
            "scrollback_lines = 10000\nscrollback_tiered_enabled = false\n"
        ))
        .is_empty());
    }

    #[test]
    fn cpu_front_ends_are_reported() {
        let software = inputs_for("front_end = \"Software\"\n");
        assert_eq!(codes(&software), ["FT-TUNE-0004"]);
        assert!(
            tuning_advisories(&software)[0].detail.contains("CPU"),
            "{:?}",
            tuning_advisories(&software)
        );

        let fallback = inputs_for("front_end = \"WebGpu\"\nwebgpu_force_fallback_adapter = true\n");
        assert_eq!(codes(&fallback), ["FT-TUNE-0004"]);
        assert_eq!(
            tuning_advisories(&fallback)[0].remediation,
            "Set webgpu_force_fallback_adapter = false (the default)."
        );

        for gpu in ["OpenGL", "WebGpu", "Metal"].iter() {
            let toml = format!("front_end = {:?}\n", gpu);
            assert!(codes(&inputs_for(&toml)).is_empty(), "{}", toml);
        }
    }

    #[test]
    fn a_gui_older_than_the_cli_is_reported() {
        let mut inputs = inputs_for("");
        inputs.cli_version = Some("0.15.21".to_string());
        inputs.gui = Some(GuiGeneration {
            source: "/Applications/FrankenTerm.app".to_string(),
            version: "0.15.6-rc.53".to_string(),
        });
        assert_eq!(codes(&inputs), ["FT-TUNE-0005"]);
        let advisory = &tuning_advisories(&inputs)[0];
        assert!(
            advisory.detail.contains(
                "/Applications/FrankenTerm.app is 0.15.6-rc.53, older than this ft (0.15.21)"
            ),
            "{}",
            advisory.detail
        );
        assert!(
            advisory
                .remediation
                .contains("install.sh --version 0.15.21"),
            "{}",
            advisory.remediation
        );

        for same_or_newer in ["FrankenTerm 0.15.21 (abc123)", "0.16.0", "not a version"].iter() {
            inputs.gui.as_mut().unwrap().version = same_or_newer.to_string();
            assert!(codes(&inputs).is_empty(), "{}", same_or_newer);
        }
        inputs.gui.as_mut().unwrap().version = "0.15.2".to_string();
        inputs.cli_version = None;
        assert!(
            codes(&inputs).is_empty(),
            "no CLI version, nothing to judge"
        );
    }

    #[test]
    fn release_versions_are_found_in_version_banners() {
        let parse = |text: &str| release_version(text).map(|version| version.to_string());
        assert_eq!(parse("0.15.21").as_deref(), Some("0.15.21"));
        assert_eq!(parse("v0.15.21").as_deref(), Some("0.15.21"));
        assert_eq!(
            parse("FrankenTerm 0.15.21 (773f5898)").as_deref(),
            Some("0.15.21")
        );
        assert_eq!(
            parse("ft 0.15.6-rc.53 (773f5898)").as_deref(),
            Some("0.15.6-rc.53")
        );
        assert_eq!(parse("FrankenTerm (unknown)"), None);
        assert!(release_version("0.15.6-rc.53") < release_version("0.15.21"));
    }

    #[test]
    fn the_operators_freeze_config_trips_the_buffer_fps_and_scrollback_rules() {
        // The settings that froze the operator (ft-yccm0.2.7), written as the
        // operator's frankenterm.lua writes them, plus tiering turned off.
        let lua = r#"
local wezterm = require 'wezterm'
local config = wezterm.config_builder()
--config.max_fps = 120
--[[
config.mux_output_parser_buffer_size = 4096
]]
config.scrollback_lines = 100000
config.mux_output_parser_buffer_size = 512 * 1024
config.front_end = "WebGpu" -- Metal is not ready yet
config.max_fps = 60
if wezterm.target_triple == "x" then config.max_fps = 30 end
config.max_fps = 30
config.scrollback_tiered_enabled = false
if config.max_fps == 30 then end
return config
"#;
        let scanned = scan_config_text(lua);
        assert_eq!(
            scanned,
            ScannedTuningSettings {
                mux_output_parser_buffer_size: Some(512 * 1024),
                max_fps: Some(30),
                scrollback_lines: Some(100_000),
                scrollback_tiered_enabled: Some(false),
                scrollback_hot_lines: None,
                front_end: Some(FrontEndSelection::WebGpu),
                webgpu_force_fallback_adapter: None,
            }
        );
        assert_eq!(
            scanned.found(),
            [
                "mux_output_parser_buffer_size",
                "max_fps",
                "scrollback_lines",
                "scrollback_tiered_enabled",
                "front_end"
            ]
        );

        let mut inputs = TuningInputs::from_config(&Config::default_config());
        scanned.apply_to(&mut inputs);
        inputs.display = display(60);
        assert_eq!(
            codes(&inputs),
            ["FT-TUNE-0001", "FT-TUNE-0002", "FT-TUNE-0003"]
        );
    }

    #[test]
    fn config_text_scanning_reads_toml_and_table_fields() {
        let toml = "# frankenterm.toml\nscrollback_lines = 100_000 # lines\n\
                    front_end = 'Software'\nwebgpu_force_fallback_adapter = true\n";
        let scanned = scan_config_text(toml);
        assert_eq!(scanned.scrollback_lines, Some(100_000));
        assert_eq!(scanned.front_end, Some(FrontEndSelection::Software));
        assert_eq!(scanned.webgpu_force_fallback_adapter, Some(true));

        let table = "return {\n  max_fps = 30,\n  scrollback_hot_lines = (2 * 1000),\n  \
                     mux_output_parser_buffer_size = compute(),\n}\n";
        let scanned = scan_config_text(table);
        assert_eq!(scanned.max_fps, Some(30));
        assert_eq!(scanned.scrollback_hot_lines, Some(2000));
        assert_eq!(
            scanned.mux_output_parser_buffer_size, None,
            "an expression that is not an integer product is not guessed"
        );
        assert_eq!(scanned.front_end, None);
    }

    #[test]
    fn published_facts_round_trip_and_judge_the_gui() {
        let dir = tempfile::tempdir().unwrap();
        let mut inputs = inputs_for("max_fps = 30\nfront_end = \"WebGpu\"\n");
        inputs.display = display(120);
        let facts = GuiTuningFacts::new(4242, "FrankenTerm 0.15.2 (319a56d)", &inputs);
        assert!(read_gui_tuning(dir.path(), 4242).unwrap().is_none());

        let publication = GuiTuningPublication::publish(dir.path(), &facts).unwrap();
        assert_eq!(publication.path(), gui_tuning_path(dir.path(), 4242));
        let read = read_gui_tuning(dir.path(), 4242).unwrap().unwrap();
        assert_eq!(read, facts);

        let judged = read.to_inputs("0.15.21");
        assert_eq!(judged.front_end, FrontEndSelection::WebGpu);
        assert_eq!(judged.display, display(120));
        assert_eq!(codes(&judged), ["FT-TUNE-0002", "FT-TUNE-0005"]);
        let generation = &tuning_advisories(&judged)[1];
        assert!(
            generation
                .detail
                .starts_with("the running GUI (pid 4242) is 0.15.2"),
            "{}",
            generation.detail
        );

        let fixed = GuiTuningFacts {
            max_fps: 120,
            ..facts
        };
        publication.republish(&fixed).unwrap();
        assert_eq!(read_gui_tuning(dir.path(), 4242).unwrap(), Some(fixed));

        drop(publication);
        assert!(
            read_gui_tuning(dir.path(), 4242).unwrap().is_none(),
            "dropping the publication removes the file"
        );
    }

    #[test]
    fn config_candidates_follow_the_gui_load_order() {
        let candidates = gui_config_candidates();
        let position = |path: PathBuf| {
            candidates
                .iter()
                .position(|candidate| *candidate == path)
                .unwrap_or_else(|| panic!("{} is not a candidate", path.display()))
        };
        let home = &*crate::HOME_DIR;
        // A frankenterm.toml beats every Lua file, as the GUI loads it first.
        assert!(position(home.join(".frankenterm.toml")) < position(home.join(".frankenterm.lua")));
        assert!(position(home.join(".frankenterm.lua")) < position(home.join(".wezterm.lua")));
        for dir in crate::CONFIG_DIRS.iter() {
            assert!(position(dir.join("frankenterm.lua")) < position(home.join(".wezterm.lua")));
            assert!(position(home.join(".wezterm.lua")) < position(dir.join("wezterm.lua")));
        }
    }

    #[test]
    fn log_line_names_the_code_setting_fix_and_docs() {
        let line = tuning_advisories(&inputs_for("max_fps = 30\n"))[0].log_line();
        assert!(
            line.starts_with("FT-TUNE-0002 max_fps: max_fps = 30 caps"),
            "{}",
            line
        );
        assert!(
            line.ends_with("Fix: Set max_fps = 60. (docs/tuning-reference.md#ft-tune-0002)"),
            "{}",
            line
        );
    }
}
