//! Setup automation for wa
//!
//! Provides idempotent patching of WezTerm configuration files and shell rc files
//! to enable wa's user-var forwarding lane and OSC 133 prompt markers.
//!
//! # Markers
//!
//! Managed blocks are identified by `WA-BEGIN` and `WA-END` markers.
//! The comment style adapts to the file type:
//! - Lua: `-- WA-BEGIN` / `-- WA-END`
//! - Shell: `# WA-BEGIN` / `# WA-END`

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::{Error, Result};

/// Marker for the start of wa-managed block (Lua style)
const WA_BEGIN_MARKER: &str = "-- WA-BEGIN (do not edit this block)";

/// Marker for the end of wa-managed block (Lua style)
const WA_END_MARKER: &str = "-- WA-END";

/// Marker for the start of wa-managed block (Shell style)
const WA_BEGIN_MARKER_SHELL: &str = "# WA-BEGIN (do not edit this block)";

/// Marker for the end of wa-managed block (Shell style)
const WA_END_MARKER_SHELL: &str = "# WA-END";

/// The Lua snippet for user-var forwarding
///
/// This snippet forwards wa-prefixed user-var events to the wa daemon.
/// See PLAN Appendix E.1 for the specification.
const USERVAR_FORWARDING_LUA: &str = r"-- Forward user-var events to wa daemon
wezterm.on('user-var-changed', function(window, pane, name, value)
  if name:match('^wa%-') then
    wezterm.background_child_process {
      'wa', 'event', '--from-uservar',
      '--pane', tostring(pane:pane_id()),
      '--name', name,
      '--value', value
    }
  end
end)";

/// The Lua snippet for status update signals
///
/// This snippet hooks WezTerm's update-status event and sends pane metadata
/// to the wa daemon. Rate-limited to 1 update per 2 seconds per pane.
/// See PLAN §2.7 for the specification.
const STATUS_UPDATE_LUA: &str = r#"-- Forward pane status updates to wa daemon (rate-limited)
local wa_last_status_update = {}
local WA_STATUS_UPDATE_INTERVAL_MS = 2000

wezterm.on('update-status', function(window, pane)
  local pane_id = pane:pane_id()
  local now_ms = os.time() * 1000

  -- Rate limit: only send if enough time has passed
  local last = wa_last_status_update[pane_id] or 0
  if now_ms - last < WA_STATUS_UPDATE_INTERVAL_MS then
    return
  end
  wa_last_status_update[pane_id] = now_ms

  -- Gather pane metadata
  local dims = pane:get_dimensions()
  local cursor = pane:get_cursor_position()
  local domain = pane:get_domain_name()
  local title = pane:get_title()
  local is_alt = pane:is_alt_screen_active()

  -- JSON-escape a string (handles \, ", and control chars)
  local function json_escape(s)
    if not s then return '' end
    return s:gsub('\\', '\\\\')
            :gsub('"', '\\"')
            :gsub('\n', '\\n')
            :gsub('\r', '\\r')
            :gsub('\t', '\\t')
  end

  -- Build JSON payload (inline to avoid dependencies)
  local payload = string.format(
    '{"schema_version":1,"pane_id":%d,"domain":"%s","title":"%s",' ..
    '"cursor":{"row":%d,"col":%d},"dimensions":{"rows":%d,"cols":%d},' ..
    '"is_alt_screen":%s,"ts":%d}',
    pane_id,
    json_escape(domain),
    json_escape(title):sub(1, 256),
    cursor.y, cursor.x,
    dims.viewport_rows, dims.viewport_cols,
    is_alt and 'true' or 'false',
    now_ms
  )

  wezterm.background_child_process {
    'wa', 'event', '--from-status',
    '--pane', tostring(pane_id),
    '--payload', payload
  }
end)"#;

// =============================================================================
// Shell Integration: OSC 133 Prompt Markers
// =============================================================================

/// Supported shell types for integration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellType {
    /// Bash shell
    Bash,
    /// Zsh shell
    Zsh,
    /// Fish shell
    Fish,
}

impl ShellType {
    /// Detect shell type from environment
    #[must_use]
    pub fn detect() -> Option<Self> {
        std::env::var("SHELL")
            .ok()
            .and_then(|s| Self::from_path(&s))
    }

    /// Parse shell type from a path (e.g., "/bin/bash")
    #[must_use]
    pub fn from_path(path: &str) -> Option<Self> {
        let name = path.rsplit('/').next()?;
        Self::from_name(name)
    }

    /// Parse shell type from name
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }

    /// Get the rc file path for this shell
    #[must_use]
    pub fn rc_file_path(&self) -> Option<PathBuf> {
        dirs::home_dir().map(|home| match self {
            Self::Bash => home.join(".bashrc"),
            Self::Zsh => home.join(".zshrc"),
            Self::Fish => home.join(".config/fish/config.fish"),
        })
    }

    /// Get the display name for this shell
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
        }
    }
}

/// OSC 133 integration snippet for Bash
///
/// Emits markers at prompt start (A), command start (C), and command end (D with exit code).
const BASH_OSC133_SNIPPET: &str = r#"# wa: OSC 133 prompt markers for deterministic state detection
# These markers help wa detect prompt boundaries and command execution.
__wa_prompt_start() { printf '\e]133;A\e\\'; }
__wa_command_start() { printf '\e]133;C\e\\'; }
__wa_command_end() { printf '\e]133;D;%s\e\\' "$__wa_last_exit"; }
__wa_preexec() {
    __wa_command_start
}
__wa_precmd() {
    __wa_last_exit=$?
    __wa_command_end
    __wa_prompt_start
}
# Install hooks if not already installed
if [[ ! "${PROMPT_COMMAND:-}" =~ __wa_precmd ]]; then
    PROMPT_COMMAND="__wa_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
fi
if [[ ! "${BASH_PREEXEC_FUNCTIONS:-}" =~ __wa_preexec ]]; then
    # bash-preexec compatible if available; otherwise use DEBUG trap
    if declare -F __bp_install > /dev/null 2>&1; then
        preexec_functions+=(__wa_preexec)
    else
        trap '__wa_preexec' DEBUG
    fi
fi"#;

/// OSC 133 integration snippet for Zsh
const ZSH_OSC133_SNIPPET: &str = r#"# wa: OSC 133 prompt markers for deterministic state detection
# These markers help wa detect prompt boundaries and command execution.
__wa_prompt_start() { printf '\e]133;A\e\\'; }
__wa_command_start() { printf '\e]133;C\e\\'; }
__wa_command_end() { printf '\e]133;D;%s\e\\' "$__wa_last_exit"; }

# Hook functions
__wa_precmd() {
    __wa_last_exit=$?
    __wa_command_end
    __wa_prompt_start
}
__wa_preexec() {
    __wa_command_start
}

# Install hooks if not already present
if [[ ! "${precmd_functions:-}" =~ __wa_precmd ]]; then
    precmd_functions+=(__wa_precmd)
fi
if [[ ! "${preexec_functions:-}" =~ __wa_preexec ]]; then
    preexec_functions+=(__wa_preexec)
fi"#;

/// OSC 133 integration snippet for Fish
const FISH_OSC133_SNIPPET: &str = r"# wa: OSC 133 prompt markers for deterministic state detection
# These markers help wa detect prompt boundaries and command execution.

function __wa_prompt_start --on-event fish_prompt
    printf '\e]133;A\e\\'
end

function __wa_command_start --on-event fish_preexec
    printf '\e]133;C\e\\'
end

function __wa_command_end --on-event fish_postexec
    printf '\e]133;D;%d\e\\' $status
end";

impl ShellType {
    /// Get the OSC 133 snippet for this shell
    #[must_use]
    pub const fn osc133_snippet(&self) -> &'static str {
        match self {
            Self::Bash => BASH_OSC133_SNIPPET,
            Self::Zsh => ZSH_OSC133_SNIPPET,
            Self::Fish => FISH_OSC133_SNIPPET,
        }
    }
}

/// Check if the shell wa-managed block is already present
#[must_use]
pub fn has_shell_wa_block(content: &str) -> bool {
    content.contains(WA_BEGIN_MARKER_SHELL) && content.contains(WA_END_MARKER_SHELL)
}

/// Create the full wa-managed block for shell rc files
fn create_shell_wa_block(shell: ShellType) -> String {
    format!(
        "{}\n{}\n{}",
        WA_BEGIN_MARKER_SHELL,
        shell.osc133_snippet(),
        WA_END_MARKER_SHELL
    )
}

/// Locate the shell rc file for the given shell type
pub fn locate_shell_rc(shell: ShellType) -> Result<PathBuf> {
    shell.rc_file_path().ok_or_else(|| {
        Error::SetupError(format!(
            "Could not determine home directory for {} rc file",
            shell.name()
        ))
    })
}

/// Idempotently patch a shell rc file with OSC 133 markers
///
/// # Behavior
///
/// - If the wa-managed block is already present, returns without modification
/// - If the block is missing, creates a backup and appends the block
/// - Creates the rc file if it doesn't exist
///
/// # Errors
///
/// Returns an error if:
/// - The home directory cannot be determined
/// - The rc file cannot be read or written
/// - Backup creation fails
pub fn patch_shell_rc(shell: ShellType) -> Result<PatchResult> {
    let rc_path = locate_shell_rc(shell)?;
    patch_shell_rc_at(&rc_path, shell)
}

/// Patch a specific shell rc file
pub fn patch_shell_rc_at(rc_path: &Path, shell: ShellType) -> Result<PatchResult> {
    // Read current content (or empty if file doesn't exist)
    let content = if rc_path.exists() {
        fs::read_to_string(rc_path).map_err(|e| {
            Error::SetupError(format!("Failed to read {}: {}", rc_path.display(), e))
        })?
    } else {
        String::new()
    };

    // Check if already patched
    if has_shell_wa_block(&content) {
        return Ok(PatchResult {
            config_path: rc_path.to_path_buf(),
            backup_path: None,
            modified: false,
            message: format!(
                "{} already contains wa OSC 133 integration. No changes needed.",
                rc_path.display()
            ),
        });
    }

    // Create backup if file exists
    let backup_path = if rc_path.exists() {
        Some(create_backup(rc_path)?)
    } else {
        // Create parent directory if needed
        if let Some(parent) = rc_path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).map_err(|e| {
                    Error::SetupError(format!(
                        "Failed to create directory {}: {}",
                        parent.display(),
                        e
                    ))
                })?;
            }
        }
        None
    };

    // Append the wa block
    let wa_block = create_shell_wa_block(shell);
    let new_content = if content.is_empty() {
        format!("{wa_block}\n")
    } else if content.ends_with('\n') {
        format!("{content}\n{wa_block}\n")
    } else {
        format!("{content}\n\n{wa_block}\n")
    };

    // Write the modified content
    fs::write(rc_path, &new_content)
        .map_err(|e| Error::SetupError(format!("Failed to write {}: {}", rc_path.display(), e)))?;

    let message = match &backup_path {
        Some(bp) => format!(
            "Added wa OSC 133 integration to {}. Backup saved to {}",
            rc_path.display(),
            bp.display()
        ),
        None => format!("Created {} with wa OSC 133 integration", rc_path.display()),
    };

    Ok(PatchResult {
        config_path: rc_path.to_path_buf(),
        backup_path,
        modified: true,
        message,
    })
}

/// Remove the wa-managed block from a shell rc file
pub fn unpatch_shell_rc_at(rc_path: &Path) -> Result<PatchResult> {
    if !rc_path.exists() {
        return Ok(PatchResult {
            config_path: rc_path.to_path_buf(),
            backup_path: None,
            modified: false,
            message: format!("{} does not exist. No changes needed.", rc_path.display()),
        });
    }

    let content = fs::read_to_string(rc_path)
        .map_err(|e| Error::SetupError(format!("Failed to read {}: {}", rc_path.display(), e)))?;

    if !has_shell_wa_block(&content) {
        return Ok(PatchResult {
            config_path: rc_path.to_path_buf(),
            backup_path: None,
            modified: false,
            message: format!(
                "{} does not contain wa block. No changes needed.",
                rc_path.display()
            ),
        });
    }

    // Create backup before modifying
    let backup_path = create_backup(rc_path)?;

    // Remove the wa block
    let begin_idx = content.find(WA_BEGIN_MARKER_SHELL).unwrap();
    let end_marker_start = content.find(WA_END_MARKER_SHELL).unwrap();
    let end_idx = content[end_marker_start..]
        .find('\n')
        .map_or(content.len(), |i| end_marker_start + i + 1);

    // Also remove any leading newlines before the block
    let mut start = begin_idx;
    while start > 0 && content.as_bytes()[start - 1] == b'\n' {
        start -= 1;
    }

    let new_content = format!("{}{}", &content[..start], &content[end_idx..]);

    fs::write(rc_path, &new_content)
        .map_err(|e| Error::SetupError(format!("Failed to write {}: {}", rc_path.display(), e)))?;

    let message = format!(
        "Removed wa block from {}. Backup saved to {}",
        rc_path.display(),
        backup_path.display()
    );

    Ok(PatchResult {
        config_path: rc_path.to_path_buf(),
        backup_path: Some(backup_path),
        modified: true,
        message,
    })
}

// =============================================================================
// SSH Config Parsing
// =============================================================================

/// Structured SSH host entry parsed from ~/.ssh/config
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    /// Host alias (the `Host` stanza name)
    pub alias: String,
    /// HostName value, if specified
    pub hostname: Option<String>,
    /// User value, if specified
    pub user: Option<String>,
    /// Port value, if specified
    pub port: Option<u16>,
    /// IdentityFile entries, in order
    pub identity_files: Vec<String>,
}

impl SshHost {
    /// Return identity file paths with redacted directories for safe display.
    #[must_use]
    pub fn redacted_identity_files(&self) -> Vec<String> {
        self.identity_files
            .iter()
            .map(|path| redact_identity_path(path))
            .collect()
    }
}

#[derive(Debug, Default, Clone)]
struct SshHostBlock {
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_files: Vec<String>,
}

/// Locate the default SSH config path (~/.ssh/config) if it exists.
pub fn locate_ssh_config() -> Result<PathBuf> {
    let path = dirs::home_dir()
        .map(|home| home.join(".ssh/config"))
        .ok_or_else(|| Error::SetupError("Could not determine home directory".to_string()))?;

    if path.exists() {
        Ok(path)
    } else {
        Err(Error::SetupError(format!(
            "SSH config not found at {}",
            path.display()
        )))
    }
}

/// Load and parse an SSH config file from disk.
pub fn load_ssh_hosts(path: &Path) -> Result<Vec<SshHost>> {
    let contents = fs::read_to_string(path)
        .map_err(|e| Error::SetupError(format!("Failed to read {}: {}", path.display(), e)))?;
    Ok(parse_ssh_config(&contents))
}

/// Parse the contents of an SSH config file into structured host entries.
#[must_use]
pub fn parse_ssh_config(contents: &str) -> Vec<SshHost> {
    let mut hosts = Vec::new();
    let mut alias_index: HashMap<String, usize> = HashMap::new();
    let mut current_aliases: Vec<String> = Vec::new();
    let mut current_block = SshHostBlock::default();

    for raw_line in contents.lines() {
        let line = strip_inline_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        let (key, raw_value) = split_key_value(line);
        if key.is_empty() {
            continue;
        }

        let key_lower = key.to_ascii_lowercase();
        if key_lower == "host" {
            flush_ssh_block(
                &mut hosts,
                &mut alias_index,
                &current_aliases,
                &current_block,
            );
            current_aliases = raw_value
                .split_whitespace()
                .filter(|alias| !is_wildcard_host(alias))
                .map(str::to_string)
                .collect();
            current_block = SshHostBlock::default();
            continue;
        }

        if current_aliases.is_empty() {
            continue;
        }

        apply_ssh_directive(&mut current_block, &key_lower, raw_value);
    }

    flush_ssh_block(
        &mut hosts,
        &mut alias_index,
        &current_aliases,
        &current_block,
    );

    hosts
}

fn apply_ssh_directive(block: &mut SshHostBlock, key: &str, value: &str) {
    let value = strip_quotes(value.trim());
    if value.is_empty() {
        return;
    }

    match key {
        "hostname" => {
            block.hostname = Some(value.to_string());
        }
        "user" => {
            block.user = Some(value.to_string());
        }
        "port" => {
            if let Ok(port) = value.parse::<u16>() {
                block.port = Some(port);
            }
        }
        "identityfile" => {
            block.identity_files.push(value.to_string());
        }
        _ => {}
    }
}

fn flush_ssh_block(
    hosts: &mut Vec<SshHost>,
    alias_index: &mut HashMap<String, usize>,
    aliases: &[String],
    block: &SshHostBlock,
) {
    if aliases.is_empty() {
        return;
    }

    for alias in aliases {
        if let Some(idx) = alias_index.get(alias).copied() {
            let host = &mut hosts[idx];
            merge_ssh_block(host, block);
            continue;
        }

        let host = SshHost {
            alias: alias.clone(),
            hostname: block.hostname.clone(),
            user: block.user.clone(),
            port: block.port,
            identity_files: block.identity_files.clone(),
        };
        alias_index.insert(alias.clone(), hosts.len());
        hosts.push(host);
    }
}

fn merge_ssh_block(host: &mut SshHost, block: &SshHostBlock) {
    if let Some(hostname) = &block.hostname {
        host.hostname = Some(hostname.clone());
    }
    if let Some(user) = &block.user {
        host.user = Some(user.clone());
    }
    if let Some(port) = block.port {
        host.port = Some(port);
    }
    if !block.identity_files.is_empty() {
        host.identity_files.clone_from(&block.identity_files);
    }
}

fn is_wildcard_host(alias: &str) -> bool {
    alias.contains('*') || alias.contains('?')
}

fn strip_inline_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn split_key_value(line: &str) -> (&str, &str) {
    let mut parts = line.splitn(2, char::is_whitespace);
    let key = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();

    if rest.is_empty() {
        if let Some((key, value)) = line.split_once('=') {
            return (key.trim(), value.trim());
        }
    }

    let rest = rest.strip_prefix('=').map_or(rest, str::trim);
    (key, rest)
}

fn strip_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..bytes.len() - 1];
        }
    }
    value
}

fn redact_identity_path(path: &str) -> String {
    let filename = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("redacted");
    if path.starts_with('~') {
        format!("~/{}", filename)
    } else {
        format!(".../{}", filename)
    }
}

// =============================================================================
// WezTerm Config Patching
// =============================================================================

/// Result of a patching operation
#[derive(Debug, Clone)]
pub struct PatchResult {
    /// Path to the config file that was patched
    pub config_path: PathBuf,
    /// Path to the backup file (if created)
    pub backup_path: Option<PathBuf>,
    /// Whether any changes were made
    pub modified: bool,
    /// Description of what happened
    pub message: String,
}

/// Locate the active WezTerm configuration file
///
/// Searches in order:
/// 1. `$XDG_CONFIG_HOME/wezterm/wezterm.lua` (or `~/.config/wezterm/wezterm.lua`)
/// 2. `~/.wezterm.lua`
///
/// Returns the first existing path, or an error if none found.
pub fn locate_wezterm_config() -> Result<PathBuf> {
    let candidates = get_config_candidates();

    for path in candidates {
        if path.exists() {
            return Ok(path);
        }
    }

    Err(Error::SetupError(
        "No WezTerm config file found. Expected ~/.config/wezterm/wezterm.lua or ~/.wezterm.lua"
            .to_string(),
    ))
}

/// Get all candidate paths for WezTerm config
fn get_config_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    // XDG config dir / wezterm / wezterm.lua
    if let Some(config_dir) = dirs::config_dir() {
        candidates.push(config_dir.join("wezterm/wezterm.lua"));
    }

    // ~/.wezterm.lua
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".wezterm.lua"));
        // Also check ~/.config/wezterm/wezterm.lua directly
        candidates.push(home.join(".config/wezterm/wezterm.lua"));
    }

    candidates
}

/// Check if the wa-managed block is already present in the content
#[must_use]
pub fn has_wa_block(content: &str) -> bool {
    content.contains(WA_BEGIN_MARKER) && content.contains(WA_END_MARKER)
}

/// Extract the current wa-managed block from content (if present)
#[must_use]
pub fn extract_wa_block(content: &str) -> Option<String> {
    let begin_idx = content.find(WA_BEGIN_MARKER)?;
    let end_idx = content.find(WA_END_MARKER)?;

    if end_idx > begin_idx {
        // Include the WA-END marker line
        let end_line_end = content[end_idx..]
            .find('\n')
            .map_or(content.len(), |i| end_idx + i);
        Some(content[begin_idx..end_line_end].to_string())
    } else {
        None
    }
}

/// Create the full wa-managed block with markers
///
/// Includes both user-var forwarding and status update snippets.
fn create_wa_block() -> String {
    format!("{WA_BEGIN_MARKER}\n{USERVAR_FORWARDING_LUA}\n\n{STATUS_UPDATE_LUA}\n{WA_END_MARKER}")
}

/// Create a backup of the config file
///
/// Backup is named `<original>.bak.<timestamp>`
fn create_backup(config_path: &Path) -> Result<PathBuf> {
    let timestamp = Utc::now().format("%Y%m%d%H%M%S");
    let backup_name = format!(
        "{}.bak.{}",
        config_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
        timestamp
    );
    let backup_path = config_path.with_file_name(backup_name);

    fs::copy(config_path, &backup_path).map_err(|e| {
        Error::SetupError(format!(
            "Failed to create backup at {}: {}",
            backup_path.display(),
            e
        ))
    })?;

    Ok(backup_path)
}

/// Idempotently patch the WezTerm config with wa's user-var forwarding snippet
///
/// # Behavior
///
/// - If the wa-managed block is already present, returns without modification
/// - If the block is missing, creates a backup and appends the block
/// - Returns a `PatchResult` describing what happened
///
/// # Errors
///
/// Returns an error if:
/// - No WezTerm config file is found
/// - The config file cannot be read or written
/// - Backup creation fails
pub fn patch_wezterm_config() -> Result<PatchResult> {
    let config_path = locate_wezterm_config()?;
    patch_wezterm_config_at(&config_path)
}

/// Patch a specific WezTerm config file
///
/// This is the internal implementation that allows specifying the path,
/// useful for testing.
pub fn patch_wezterm_config_at(config_path: &Path) -> Result<PatchResult> {
    // Read current content
    let content = fs::read_to_string(config_path).map_err(|e| {
        Error::SetupError(format!("Failed to read {}: {}", config_path.display(), e))
    })?;

    // Check if already patched
    if has_wa_block(&content) {
        return Ok(PatchResult {
            config_path: config_path.to_path_buf(),
            backup_path: None,
            modified: false,
            message: "WezTerm config already contains wa user-var forwarding. No changes needed."
                .to_string(),
        });
    }

    // Create backup before modifying
    let backup_path = create_backup(config_path)?;

    // Append the wa block
    let wa_block = create_wa_block();
    let new_content = if content.ends_with('\n') {
        format!("{content}\n{wa_block}\n")
    } else {
        format!("{content}\n\n{wa_block}\n")
    };

    // Write the modified content
    fs::write(config_path, &new_content).map_err(|e| {
        Error::SetupError(format!("Failed to write {}: {}", config_path.display(), e))
    })?;

    let backup_display = backup_path.display().to_string();
    let message = format!(
        "Added wa user-var forwarding to {}. Backup saved to {}",
        config_path.display(),
        backup_display
    );

    Ok(PatchResult {
        config_path: config_path.to_path_buf(),
        backup_path: Some(backup_path),
        modified: true,
        message,
    })
}

/// Remove the wa-managed block from a WezTerm config file
///
/// This is useful for uninstalling or resetting.
pub fn unpatch_wezterm_config_at(config_path: &Path) -> Result<PatchResult> {
    let content = fs::read_to_string(config_path).map_err(|e| {
        Error::SetupError(format!("Failed to read {}: {}", config_path.display(), e))
    })?;

    if !has_wa_block(&content) {
        return Ok(PatchResult {
            config_path: config_path.to_path_buf(),
            backup_path: None,
            modified: false,
            message: "WezTerm config does not contain wa block. No changes needed.".to_string(),
        });
    }

    // Create backup before modifying
    let backup_path = create_backup(config_path)?;

    // Remove the wa block
    let begin_idx = content.find(WA_BEGIN_MARKER).unwrap();
    let end_marker_start = content.find(WA_END_MARKER).unwrap();
    let end_idx = content[end_marker_start..]
        .find('\n')
        .map_or(content.len(), |i| end_marker_start + i + 1);

    // Also remove any leading newlines before the block
    let mut start = begin_idx;
    while start > 0 && content.as_bytes()[start - 1] == b'\n' {
        start -= 1;
    }

    let new_content = format!("{}{}", &content[..start], &content[end_idx..]);

    fs::write(config_path, &new_content).map_err(|e| {
        Error::SetupError(format!("Failed to write {}: {}", config_path.display(), e))
    })?;

    let backup_display = backup_path.display().to_string();
    let message = format!(
        "Removed wa block from {}. Backup saved to {}",
        config_path.display(),
        backup_display
    );

    Ok(PatchResult {
        config_path: config_path.to_path_buf(),
        backup_path: Some(backup_path),
        modified: true,
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn create_temp_config(content: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", content).unwrap();
        file
    }

    #[test]
    fn test_has_wa_block_when_present() {
        let content = r"
local wezterm = require 'wezterm'
config = {}

-- WA-BEGIN (do not edit this block)
-- some wa code
-- WA-END

return config
";
        assert!(has_wa_block(content));
    }

    #[test]
    fn test_has_wa_block_when_absent() {
        let content = r"
local wezterm = require 'wezterm'
config = {}
return config
";
        assert!(!has_wa_block(content));
    }

    #[test]
    fn test_has_wa_block_partial_markers() {
        // Only BEGIN marker
        let content1 = "-- WA-BEGIN (do not edit this block)\nsome code";
        assert!(!has_wa_block(content1));

        // Only END marker
        let content2 = "some code\n-- WA-END";
        assert!(!has_wa_block(content2));
    }

    #[test]
    fn test_patch_inserts_block() {
        let original = r"local wezterm = require 'wezterm'
local config = {}
return config
";
        let file = create_temp_config(original);

        let result = patch_wezterm_config_at(file.path()).unwrap();

        assert!(result.modified);
        assert!(result.backup_path.is_some());

        let patched = fs::read_to_string(file.path()).unwrap();
        assert!(has_wa_block(&patched));
        assert!(patched.contains("wezterm.on('user-var-changed'"));
        assert!(patched.contains("wa%-"));
    }

    #[test]
    fn test_patch_is_idempotent() {
        let original = r"local wezterm = require 'wezterm'
local config = {}

-- WA-BEGIN (do not edit this block)
-- Forward user-var events to wa daemon
wezterm.on('user-var-changed', function(window, pane, name, value)
  if name:match('^wa%-') then
    wezterm.background_child_process {
      'wa', 'event', '--from-uservar',
      '--pane', tostring(pane:pane_id()),
      '--name', name,
      '--value', value
    }
  end
end)
-- WA-END

return config
";
        let file = create_temp_config(original);

        let result = patch_wezterm_config_at(file.path()).unwrap();

        assert!(!result.modified);
        assert!(result.backup_path.is_none());

        // Content should be unchanged
        let content_after = fs::read_to_string(file.path()).unwrap();
        assert_eq!(original, content_after);
    }

    #[test]
    fn test_backup_is_created() {
        let original = "local wezterm = require 'wezterm'\n";
        let file = create_temp_config(original);

        let result = patch_wezterm_config_at(file.path()).unwrap();

        assert!(result.modified);
        let backup_path = result.backup_path.unwrap();
        assert!(backup_path.exists());

        // Backup should contain original content
        let backup_content = fs::read_to_string(&backup_path).unwrap();
        assert_eq!(original, backup_content);
    }

    #[test]
    fn test_unpatch_removes_block() {
        let with_block = r"local wezterm = require 'wezterm'
local config = {}

-- WA-BEGIN (do not edit this block)
-- some wa code
-- WA-END

return config
";
        let file = create_temp_config(with_block);

        let result = unpatch_wezterm_config_at(file.path()).unwrap();

        assert!(result.modified);
        let unpatched = fs::read_to_string(file.path()).unwrap();
        assert!(!has_wa_block(&unpatched));
        assert!(unpatched.contains("return config"));
    }

    #[test]
    fn test_unpatch_is_idempotent() {
        let without_block = "local wezterm = require 'wezterm'\n";
        let file = create_temp_config(without_block);

        let result = unpatch_wezterm_config_at(file.path()).unwrap();

        assert!(!result.modified);
        assert!(result.backup_path.is_none());
    }

    #[test]
    fn test_extract_wa_block() {
        let content = r"before
-- WA-BEGIN (do not edit this block)
-- code here
-- WA-END
after";

        let block = extract_wa_block(content).unwrap();
        assert!(block.starts_with("-- WA-BEGIN"));
        assert!(block.contains("-- code here"));
        assert!(block.ends_with("-- WA-END"));
    }

    #[test]
    fn test_create_wa_block_format() {
        let block = create_wa_block();

        assert!(block.starts_with(WA_BEGIN_MARKER));
        assert!(block.ends_with(WA_END_MARKER));
        // User-var forwarding snippet
        assert!(block.contains("user-var-changed"));
        assert!(block.contains("wa%-"));
        // Status update snippet
        assert!(block.contains("update-status"));
        assert!(block.contains("wa_last_status_update"));
        assert!(block.contains("WA_STATUS_UPDATE_INTERVAL_MS"));
        assert!(block.contains("--from-status"));
        // JSON escaping helper
        assert!(block.contains("json_escape"));
    }

    // =========================================================================
    // Shell Integration Tests
    // =========================================================================

    #[test]
    fn test_has_shell_wa_block_when_present() {
        let content = r"# existing bashrc content
export PATH=$HOME/bin:$PATH

# WA-BEGIN (do not edit this block)
# wa: OSC 133 prompt markers
__wa_prompt_start() { printf '\e]133;A\e\\'; }
# WA-END

# more user config
";
        assert!(has_shell_wa_block(content));
    }

    #[test]
    fn test_has_shell_wa_block_when_absent() {
        let content = r"# existing bashrc content
export PATH=$HOME/bin:$PATH
alias ll='ls -la'
";
        assert!(!has_shell_wa_block(content));
    }

    #[test]
    fn test_has_shell_wa_block_partial_markers() {
        // Only BEGIN marker
        let content1 = "# WA-BEGIN (do not edit this block)\nsome code";
        assert!(!has_shell_wa_block(content1));

        // Only END marker
        let content2 = "some code\n# WA-END";
        assert!(!has_shell_wa_block(content2));
    }

    #[test]
    fn test_shell_patch_inserts_block() {
        let original = r"# ~/.bashrc
export PATH=$HOME/bin:$PATH
";
        let file = create_temp_config(original);

        let result = patch_shell_rc_at(file.path(), ShellType::Bash).unwrap();

        assert!(result.modified);
        assert!(result.backup_path.is_some());

        let patched = fs::read_to_string(file.path()).unwrap();
        assert!(has_shell_wa_block(&patched));
        assert!(patched.contains("OSC 133"));
        assert!(patched.contains("__wa_prompt_start"));
        assert!(patched.contains("__wa_precmd"));
    }

    #[test]
    fn test_shell_patch_is_idempotent() {
        let original = r"# ~/.bashrc
export PATH=$HOME/bin:$PATH

# WA-BEGIN (do not edit this block)
# wa: OSC 133 prompt markers for deterministic state detection
__wa_prompt_start() { printf '\e]133;A\e\\'; }
__wa_command_start() { printf '\e]133;C\e\\'; }
# WA-END

alias ll='ls -la'
";
        let file = create_temp_config(original);

        let result = patch_shell_rc_at(file.path(), ShellType::Bash).unwrap();

        assert!(!result.modified);
        assert!(result.backup_path.is_none());

        // Content should be unchanged
        let content_after = fs::read_to_string(file.path()).unwrap();
        assert_eq!(original, content_after);
    }

    #[test]
    fn test_shell_unpatch_removes_block() {
        let with_block = r"# ~/.bashrc
export PATH=$HOME/bin:$PATH

# WA-BEGIN (do not edit this block)
# wa: OSC 133 markers
__wa_prompt_start() { printf '\e]133;A\e\\'; }
# WA-END

alias ll='ls -la'
";
        let file = create_temp_config(with_block);

        let result = unpatch_shell_rc_at(file.path()).unwrap();

        assert!(result.modified);
        assert!(result.backup_path.is_some());
        let unpatched = fs::read_to_string(file.path()).unwrap();
        assert!(!has_shell_wa_block(&unpatched));
        assert!(unpatched.contains("alias ll"));
    }

    #[test]
    fn test_shell_unpatch_nonexistent_file() {
        let path = std::path::Path::new("/tmp/nonexistent_file_wa_test_12345.bashrc");
        let result = unpatch_shell_rc_at(path).unwrap();

        assert!(!result.modified);
        assert!(result.backup_path.is_none());
    }

    #[test]
    fn test_shell_type_from_path() {
        assert_eq!(ShellType::from_path("/bin/bash"), Some(ShellType::Bash));
        assert_eq!(ShellType::from_path("/usr/bin/zsh"), Some(ShellType::Zsh));
        assert_eq!(
            ShellType::from_path("/usr/local/bin/fish"),
            Some(ShellType::Fish)
        );
        assert_eq!(ShellType::from_path("/bin/sh"), None);
        assert_eq!(ShellType::from_path("/bin/dash"), None);
    }

    #[test]
    fn test_shell_type_from_name() {
        assert_eq!(ShellType::from_name("bash"), Some(ShellType::Bash));
        assert_eq!(ShellType::from_name("BASH"), Some(ShellType::Bash));
        assert_eq!(ShellType::from_name("zsh"), Some(ShellType::Zsh));
        assert_eq!(ShellType::from_name("fish"), Some(ShellType::Fish));
        assert_eq!(ShellType::from_name("sh"), None);
    }

    #[test]
    fn test_shell_type_name() {
        assert_eq!(ShellType::Bash.name(), "bash");
        assert_eq!(ShellType::Zsh.name(), "zsh");
        assert_eq!(ShellType::Fish.name(), "fish");
    }

    #[test]
    fn test_shell_osc133_snippets_differ() {
        // Each shell should have a unique snippet
        let bash = ShellType::Bash.osc133_snippet();
        let zsh = ShellType::Zsh.osc133_snippet();
        let fish = ShellType::Fish.osc133_snippet();

        assert_ne!(bash, zsh);
        assert_ne!(bash, fish);
        assert_ne!(zsh, fish);

        // All should contain the OSC 133 escape sequence
        assert!(bash.contains("133;A"));
        assert!(zsh.contains("133;A"));
        assert!(fish.contains("133;A"));
    }

    #[test]
    fn test_shell_patch_creates_file_if_missing() {
        let temp_dir = tempfile::tempdir().unwrap();
        let rc_path = temp_dir.path().join("test.bashrc");

        // File doesn't exist yet
        assert!(!rc_path.exists());

        let result = patch_shell_rc_at(&rc_path, ShellType::Bash).unwrap();

        assert!(result.modified);
        assert!(result.backup_path.is_none()); // No backup for new file
        assert!(rc_path.exists());

        let content = fs::read_to_string(&rc_path).unwrap();
        assert!(has_shell_wa_block(&content));
    }

    #[test]
    fn test_shell_patch_creates_parent_dirs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let rc_path = temp_dir.path().join("subdir/deep/config.fish");

        // Parent dirs don't exist
        assert!(!rc_path.parent().unwrap().exists());

        let result = patch_shell_rc_at(&rc_path, ShellType::Fish).unwrap();

        assert!(result.modified);
        assert!(rc_path.exists());

        let content = fs::read_to_string(&rc_path).unwrap();
        assert!(has_shell_wa_block(&content));
        // Fish snippet should have fish-specific syntax
        assert!(content.contains("--on-event fish_prompt"));
    }

    #[test]
    fn parse_ssh_config_basic_fixture() {
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ssh_config/basic_config"
        ));
        let hosts = parse_ssh_config(fixture);

        let aliases: Vec<_> = hosts.iter().map(|host| host.alias.as_str()).collect();
        assert_eq!(aliases, vec!["prod", "staging", "dev"]);
        assert!(
            hosts
                .iter()
                .all(|host| !host.alias.contains('*') && !host.alias.contains('?'))
        );

        let prod = &hosts[0];
        assert_eq!(prod.hostname.as_deref(), Some("prod.example.com"));
        assert_eq!(prod.user.as_deref(), Some("ubuntu"));
        assert_eq!(prod.port, Some(2222));
        assert_eq!(
            prod.identity_files,
            vec!["~/.ssh/id_ed25519", "~/.ssh/id_ed25519_work"]
        );
    }

    #[test]
    fn parse_ssh_config_comments_fixture() {
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ssh_config/comments_config"
        ));
        let hosts = parse_ssh_config(fixture);
        assert_eq!(hosts.len(), 1);

        let host = &hosts[0];
        assert_eq!(host.alias, "test");
        assert_eq!(host.hostname.as_deref(), Some("test.example.com"));
        assert_eq!(host.user.as_deref(), Some("alice"));
        assert_eq!(host.port, Some(2200));
        assert_eq!(host.identity_files, vec!["~/.ssh/id_rsa"]);
    }
}
