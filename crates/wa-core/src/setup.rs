//! Setup automation for wa
//!
//! Provides idempotent patching of WezTerm configuration files and shell rc files
//! to enable wa's user-var forwarding lane and OSC 133 prompt markers.
//!
//! # Architecture (v0.2.0+)
//!
//! The WezTerm integration uses a polling-based architecture:
//! - **Pane metadata**: Obtained via `wezterm cli list` only when needed
//! - **Alt-screen detection**: Via escape sequence parsing (see `screen_state.rs`)
//! - **User-var signaling**: Via Lua `user-var-changed` hook (still active)
//!
//! The Lua `update-status` hook was removed in v0.2.0 due to performance issues
//! (it fired at ~60Hz, causing continuous Lua interpreter invocations and IPC overhead).
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

// NOTE: STATUS_UPDATE_LUA was removed in v0.2.0 to eliminate Lua performance bottleneck.
// The update-status event fires at ~60Hz, causing significant WezTerm slowdown.
// Alt-screen detection is now handled via escape sequence parsing (see screen_state.rs).
// Pane metadata (title, dimensions, cursor) is obtained via `wezterm cli list`.

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
    } else if path.contains('/') || path.contains('\\') {
        format!(".../{}", filename)
    } else {
        path.to_string()
    }
}

/// Generate a WA-managed wezterm.lua block for ssh_domains.
#[must_use]
pub fn generate_ssh_domains_lua(hosts: &[SshHost], scrollback_lines: u64) -> String {
    let mut output = String::new();
    output.push_str(WA_BEGIN_MARKER);
    output.push('\n');
    output.push_str("-- wa: generated ssh_domains config\n");
    output.push_str("config = config or {}\n");
    output.push_str(&format!("config.scrollback_lines = {scrollback_lines}\n\n"));
    // Preserve any existing ssh_domains defined outside the WA block
    output.push_str("config.ssh_domains = config.ssh_domains or {}\n");
    if hosts.is_empty() {
        output.push_str(
            "-- No SSH hosts found; add entries manually or re-run wa setup --list-hosts\n",
        );
    } else {
        output.push_str("local wa_ssh_domains = {\n");

        for host in hosts {
            let name = lua_escape(&host.alias);
            let remote = lua_escape(host.hostname.as_deref().unwrap_or(&host.alias));
            output.push_str("  {\n");
            output.push_str(&format!("    name = '{name}',\n"));
            output.push_str(&format!("    remote_address = '{remote}',\n"));
            if let Some(user) = host.user.as_deref() {
                output.push_str(&format!("    username = '{}',\n", lua_escape(user)));
            }
            if let Some(port) = host.port {
                output.push_str(&format!("    port = {},\n", port));
            }
            // Emit identity files so WezTerm uses the correct SSH keys (#15).
            // ssh_option is a Lua table (key-value), so duplicate keys aren't
            // possible. Use the first identity file (SSH config tries them in
            // order, so the first is typically the most specific for this host).
            if let Some(ifile) = host.identity_files.first() {
                output.push_str("    ssh_option = {\n");
                output.push_str(&format!("      identityfile = '{}',\n", lua_escape(ifile)));
                output.push_str("    },\n");
            }
            output.push_str("    multiplexing = 'WezTerm',\n");
            output.push_str("  },\n");
        }

        output.push_str("}\n");
        // Append WA-managed domains instead of overwriting (#16)
        output.push_str("for _, domain in ipairs(wa_ssh_domains) do\n");
        output.push_str("  table.insert(config.ssh_domains, domain)\n");
        output.push_str("end\n");
    }
    output.push('\n');
    output.push_str(USERVAR_FORWARDING_LUA);
    output.push('\n');
    output.push_str(WA_END_MARKER);
    output.push('\n');
    output
}

fn lua_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
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

fn find_return_line_start(content: &str) -> Option<usize> {
    let mut offset = 0usize;
    let mut last_match = None;

    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed == "return" || trimmed.starts_with("return ") {
            last_match = Some(offset);
        }
        offset = offset.saturating_add(line.len() + 1);
    }

    last_match
}

fn insert_wa_block(content: &str, wa_block: &str) -> String {
    let normalized_block = wa_block.trim_end_matches('\n');
    if let Some(return_idx) = find_return_line_start(content) {
        let before = content[..return_idx].trim_end_matches('\n');
        let after = content[return_idx..].trim_start_matches('\n');
        format!("{before}\n\n{normalized_block}\n\n{after}")
    } else if content.ends_with('\n') {
        format!("{content}\n{normalized_block}\n")
    } else {
        format!("{content}\n\n{normalized_block}\n")
    }
}

/// Create the full wa-managed block with markers
///
/// Includes user-var forwarding for SSH domain support.
/// Note: Status update Lua was removed in v0.2.0 to eliminate performance bottleneck.
fn create_wa_block() -> String {
    format!("{WA_BEGIN_MARKER}\n{USERVAR_FORWARDING_LUA}\n{WA_END_MARKER}")
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

    let wa_block = create_wa_block();

    // Check if already patched
    if has_wa_block(&content) {
        let existing = extract_wa_block(&content).unwrap_or_default();
        let normalized_existing = existing.trim_end_matches('\n');
        let normalized_new = wa_block.trim_end_matches('\n');

        if normalized_existing == normalized_new {
            return Ok(PatchResult {
                config_path: config_path.to_path_buf(),
                backup_path: None,
                modified: false,
                message:
                    "WezTerm config already contains wa user-var forwarding. No changes needed."
                        .to_string(),
            });
        }

        let contains_config_block = existing.contains("config.ssh_domains")
            || existing.contains("config.scrollback_lines")
            || existing.contains("config = config or {}");

        if contains_config_block {
            return Ok(PatchResult {
                config_path: config_path.to_path_buf(),
                backup_path: None,
                modified: false,
                message: "WezTerm config already contains a wa block managed by `wa setup config`. Re-run `wa setup config --apply` to update it."
                    .to_string(),
            });
        }

        let legacy_status =
            existing.contains("update-status") || existing.contains("--from-status");
        if legacy_status {
            return patch_wezterm_config_block_at(config_path, &wa_block);
        }

        return Ok(PatchResult {
            config_path: config_path.to_path_buf(),
            backup_path: None,
            modified: false,
            message: "WezTerm config already contains a wa block. No changes needed.".to_string(),
        });
    }

    // Create backup before modifying
    let backup_path = create_backup(config_path)?;

    // Append the wa block (insert before return if present)
    let new_content = insert_wa_block(&content, &wa_block);

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

/// Patch a WezTerm config file with a specific wa-managed block.
///
/// This supports idempotent updates for generated blocks (e.g., ssh_domains).
pub fn patch_wezterm_config_block_at(config_path: &Path, wa_block: &str) -> Result<PatchResult> {
    if !wa_block.contains(WA_BEGIN_MARKER) || !wa_block.contains(WA_END_MARKER) {
        return Err(Error::SetupError(
            "Generated wa block is missing WA markers.".to_string(),
        ));
    }

    let content = fs::read_to_string(config_path).map_err(|e| {
        Error::SetupError(format!("Failed to read {}: {}", config_path.display(), e))
    })?;

    let normalized_block = wa_block.trim_end_matches('\n');

    if has_wa_block(&content) {
        let existing = extract_wa_block(&content).unwrap_or_default();
        let normalized_existing = existing.trim_end_matches('\n');
        if normalized_existing == normalized_block {
            return Ok(PatchResult {
                config_path: config_path.to_path_buf(),
                backup_path: None,
                modified: false,
                message:
                    "WezTerm config already contains an up-to-date wa block. No changes needed."
                        .to_string(),
            });
        }

        let backup_path = create_backup(config_path)?;

        let begin_idx = content.find(WA_BEGIN_MARKER).unwrap();
        let end_marker_start = content.find(WA_END_MARKER).unwrap();
        let end_idx = content[end_marker_start..]
            .find('\n')
            .map_or(content.len(), |i| end_marker_start + i + 1);

        let return_idx = find_return_line_start(&content);
        let new_content = if return_idx.is_some_and(|idx| begin_idx > idx) {
            let without_block = format!("{}{}", &content[..begin_idx], &content[end_idx..]);
            insert_wa_block(&without_block, normalized_block)
        } else {
            format!(
                "{}{}\n{}",
                &content[..begin_idx],
                normalized_block,
                &content[end_idx..]
            )
        };

        fs::write(config_path, &new_content).map_err(|e| {
            Error::SetupError(format!("Failed to write {}: {}", config_path.display(), e))
        })?;

        let backup_display = backup_path.display().to_string();
        let message = format!(
            "Updated wa block in {}. Backup saved to {}",
            config_path.display(),
            backup_display
        );

        return Ok(PatchResult {
            config_path: config_path.to_path_buf(),
            backup_path: Some(backup_path),
            modified: true,
            message,
        });
    }

    let backup_path = create_backup(config_path)?;

    let new_content = insert_wa_block(&content, normalized_block);

    fs::write(config_path, &new_content).map_err(|e| {
        Error::SetupError(format!("Failed to write {}: {}", config_path.display(), e))
    })?;

    let backup_display = backup_path.display().to_string();
    let message = format!(
        "Added wa block to {}. Backup saved to {}",
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

// =============================================================================
// Setup Wizard: Guided First-Run Configuration
// =============================================================================

use crate::config::Config;
use crate::environment::{AutoConfig, ConfigRecommendation, DetectedEnvironment};

/// Result of a single wizard detection step.
#[derive(Debug, Clone)]
pub struct DetectionStep {
    /// Display label (e.g. "WezTerm CLI")
    pub label: String,
    /// Whether the check passed
    pub ok: bool,
    /// Human-readable detail
    pub detail: String,
}

/// Wizard configuration choice made by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardChoice {
    /// Accept auto-detected recommendations as-is
    Accept,
    /// Skip setup entirely (use defaults)
    Skip,
}

/// Result of running the setup wizard.
#[derive(Debug, Clone)]
pub struct WizardResult {
    /// Detection steps that were executed
    pub steps: Vec<DetectionStep>,
    /// Auto-config recommendations
    pub recommendations: Vec<ConfigRecommendation>,
    /// Generated config (if not skipped)
    pub config: Option<Config>,
    /// Path where config was saved (if any)
    pub config_path: Option<PathBuf>,
    /// Patches applied (WezTerm, shell)
    pub patches: Vec<PatchResult>,
}

/// Guided first-run setup wizard.
///
/// Uses [`DetectedEnvironment`] and [`AutoConfig`] to probe the system
/// and generate an optimal `wa.toml`.
pub struct SetupWizard {
    env: DetectedEnvironment,
    auto: AutoConfig,
}

impl SetupWizard {
    /// Create a wizard from a pre-detected environment.
    #[must_use]
    pub fn new(env: DetectedEnvironment) -> Self {
        let auto = AutoConfig::from_environment(&env);
        Self { env, auto }
    }

    /// Run the detection phase and return human-readable steps.
    #[must_use]
    pub fn detect(&self) -> Vec<DetectionStep> {
        let mut steps = Vec::new();

        // WezTerm CLI
        if let Some(ref ver) = self.env.wezterm.version {
            steps.push(DetectionStep {
                label: "WezTerm CLI".into(),
                ok: true,
                detail: format!("{ver} detected"),
            });
        } else {
            steps.push(DetectionStep {
                label: "WezTerm CLI".into(),
                ok: false,
                detail: "not found in PATH".into(),
            });
        }

        // WezTerm socket
        if let Some(ref sock) = self.env.wezterm.socket_path {
            steps.push(DetectionStep {
                label: "Socket".into(),
                ok: true,
                detail: format!("found at {}", sock.display()),
            });
        }

        // Shell
        if let Some(ref shell_type) = self.env.shell.shell_type {
            let ver_suffix = self
                .env
                .shell
                .version
                .as_deref()
                .map(|v| format!(" {v}"))
                .unwrap_or_default();
            steps.push(DetectionStep {
                label: "Shell".into(),
                ok: true,
                detail: format!("{shell_type}{ver_suffix}"),
            });
        } else {
            steps.push(DetectionStep {
                label: "Shell".into(),
                ok: false,
                detail: "could not detect from $SHELL".into(),
            });
        }

        // OSC 133
        if self.env.shell.osc_133_enabled {
            steps.push(DetectionStep {
                label: "OSC 133".into(),
                ok: true,
                detail: "enabled".into(),
            });
        } else {
            steps.push(DetectionStep {
                label: "OSC 133".into(),
                ok: false,
                detail: "not enabled (optional but recommended)".into(),
            });
        }

        // Running panes
        if self.env.wezterm.is_running {
            steps.push(DetectionStep {
                label: "Panes".into(),
                ok: true,
                detail: "WezTerm responding".into(),
            });
        }

        // Detected agents
        for agent in &self.env.agents {
            steps.push(DetectionStep {
                label: "Agent".into(),
                ok: true,
                detail: format!("{:?} in pane {}", agent.agent_type, agent.pane_id),
            });
        }

        // Remote hosts
        for remote in &self.env.remotes {
            steps.push(DetectionStep {
                label: "Remote".into(),
                ok: true,
                detail: format!(
                    "{} ({:?}, {} pane(s))",
                    remote.hostname,
                    remote.connection_type,
                    remote.pane_ids.len()
                ),
            });
        }

        // System summary
        let mem_str = self
            .env
            .system
            .memory_mb
            .map(|mb| format!(", {mb} MB RAM"))
            .unwrap_or_default();
        steps.push(DetectionStep {
            label: "System".into(),
            ok: true,
            detail: format!(
                "{} {} ({} CPUs{})",
                self.env.system.os, self.env.system.arch, self.env.system.cpu_count, mem_str
            ),
        });

        steps
    }

    /// Access the auto-configuration recommendations.
    #[must_use]
    pub fn recommendations(&self) -> &[ConfigRecommendation] {
        &self.auto.recommendations
    }

    /// Access the auto-config.
    #[must_use]
    pub fn auto_config(&self) -> &AutoConfig {
        &self.auto
    }

    /// Access the detected environment.
    #[must_use]
    pub fn environment(&self) -> &DetectedEnvironment {
        &self.env
    }

    /// Generate a [`Config`] from the auto-detected settings.
    #[must_use]
    pub fn generate_config(&self) -> Config {
        let mut config = Config::default();
        config.ingest.poll_interval_ms = self.auto.poll_interval_ms;
        config.ingest.min_poll_interval_ms = self.auto.min_poll_interval_ms;
        config.ingest.max_concurrent_captures = self.auto.max_concurrent_captures;
        config.patterns.packs.clone_from(&self.auto.pattern_packs);
        config.safety.rate_limit_per_pane = self.auto.rate_limit_per_pane;
        config
    }

    /// Build the full wizard result.
    ///
    /// `choice` controls whether a config is generated.
    /// When `apply_patches` is true, WezTerm and shell configs are patched.
    pub fn finish(
        &self,
        choice: WizardChoice,
        apply_patches: bool,
        config_save_path: Option<&Path>,
    ) -> Result<WizardResult> {
        let steps = self.detect();
        let recommendations = self.auto.recommendations.clone();
        let mut patches = Vec::new();

        let config = match choice {
            WizardChoice::Accept => Some(self.generate_config()),
            WizardChoice::Skip => None,
        };

        // Optionally save config
        let config_path = if let Some(ref cfg) = config {
            if let Some(path) = config_save_path {
                let toml_str = cfg.to_toml()?;
                if let Some(parent) = path.parent() {
                    if !parent.exists() {
                        fs::create_dir_all(parent).map_err(|e| {
                            Error::SetupError(format!(
                                "Failed to create config directory {}: {}",
                                parent.display(),
                                e
                            ))
                        })?;
                    }
                }
                fs::write(path, &toml_str).map_err(|e| {
                    Error::SetupError(format!(
                        "Failed to write config to {}: {}",
                        path.display(),
                        e
                    ))
                })?;
                Some(path.to_path_buf())
            } else {
                None
            }
        } else {
            None
        };

        // Optionally apply patches
        if apply_patches {
            // WezTerm config
            if let Ok(wez_path) = locate_wezterm_config() {
                if let Ok(result) = patch_wezterm_config_at(&wez_path) {
                    patches.push(result);
                }
            }

            // Shell rc
            if let Some(shell_type) = ShellType::detect() {
                if let Ok(result) = patch_shell_rc(shell_type) {
                    patches.push(result);
                }
            }
        }

        Ok(WizardResult {
            steps,
            recommendations,
            config,
            config_path,
            patches,
        })
    }
}

/// Return the default config save path (~/.config/wa/wa.toml or platform equivalent).
#[must_use]
pub fn default_config_save_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| {
            h.join("Library")
                .join("Application Support")
                .join("wa")
                .join("wa.toml")
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
            .map(|p| p.join("wa").join("wa.toml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn setup_fixture(name: &str) -> &'static str {
        match name {
            "wezterm_missing.lua" => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/setup/wezterm_missing.lua"
            )),
            "wezterm_present.lua" => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/setup/wezterm_present.lua"
            )),
            "shell_missing.bashrc" => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/setup/shell_missing.bashrc"
            )),
            "shell_present.bashrc" => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/setup/shell_present.bashrc"
            )),
            _ => panic!("Unknown setup fixture: {name}"),
        }
    }

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
        let wa_idx = patched.find(WA_BEGIN_MARKER).unwrap();
        let return_idx = patched.find("return config").unwrap();
        assert!(wa_idx < return_idx);
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
    fn test_patch_upgrades_legacy_status_update_block() {
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
-- Forward pane status updates to wa daemon (rate-limited)
wezterm.on('update-status', function(window, pane)
  wezterm.background_child_process { 'wa', 'event', '--from-status' }
end)
-- WA-END

return config
";
        let file = create_temp_config(original);

        let result = patch_wezterm_config_at(file.path()).unwrap();

        assert!(result.modified);
        assert!(result.backup_path.is_some());

        let content_after = fs::read_to_string(file.path()).unwrap();
        assert!(content_after.contains("user-var-changed"));
        assert!(!content_after.contains("update-status"));
        assert!(!content_after.contains("--from-status"));
    }

    #[test]
    fn test_generate_ssh_domains_block_includes_hosts_and_snippets() {
        let hosts = vec![SshHost {
            alias: "box".to_string(),
            hostname: Some("box.example".to_string()),
            user: Some("alice".to_string()),
            port: Some(2222),
            identity_files: Vec::new(),
        }];

        let block = generate_ssh_domains_lua(&hosts, 50_000);
        assert!(block.contains(WA_BEGIN_MARKER));
        assert!(block.contains("config = config or {}"));
        assert!(block.contains("config.scrollback_lines = 50000"));
        assert!(block.contains("config.ssh_domains = config.ssh_domains or {}"));
        assert!(block.contains("local wa_ssh_domains = {"));
        assert!(block.contains("name = 'box'"));
        assert!(block.contains("remote_address = 'box.example'"));
        assert!(block.contains("username = 'alice'"));
        assert!(block.contains("port = 2222"));
        assert!(block.contains("multiplexing = 'WezTerm'"));
        assert!(block.contains(USERVAR_FORWARDING_LUA));
        // Note: STATUS_UPDATE_LUA was removed in v0.2.0 (alt-screen now via escape sequences)
        assert!(block.contains(WA_END_MARKER));
        // Verify additive append pattern (not overwrite)
        assert!(block.contains("table.insert(config.ssh_domains, domain)"));
    }

    #[test]
    fn test_generate_ssh_domains_includes_identity_file() {
        let hosts = vec![SshHost {
            alias: "secure".to_string(),
            hostname: Some("secure.example".to_string()),
            user: Some("deploy".to_string()),
            port: None,
            identity_files: vec![
                "~/.ssh/id_ed25519_deploy".to_string(),
                "~/.ssh/id_rsa_backup".to_string(),
            ],
        }];
        let block = generate_ssh_domains_lua(&hosts, 50_000);
        // Should include first identity file in ssh_option
        assert!(block.contains("ssh_option = {"));
        assert!(block.contains("identityfile = '~/.ssh/id_ed25519_deploy'"));
        // Should NOT include second file (Lua table keys must be unique)
        assert!(!block.contains("id_rsa_backup"));
    }

    #[test]
    fn test_patch_wezterm_config_block_inserts_before_return() {
        let original = r"local wezterm = require 'wezterm'
local config = {}
return config
";
        let file = create_temp_config(original);
        let hosts = vec![SshHost {
            alias: "alpha".to_string(),
            hostname: Some("alpha.example".to_string()),
            user: None,
            port: None,
            identity_files: Vec::new(),
        }];
        let block = generate_ssh_domains_lua(&hosts, 50_000);

        let result = patch_wezterm_config_block_at(file.path(), &block).unwrap();
        assert!(result.modified);

        let patched = fs::read_to_string(file.path()).unwrap();
        let wa_idx = patched.find(WA_BEGIN_MARKER).unwrap();
        let return_idx = patched.find("return config").unwrap();
        assert!(wa_idx < return_idx);
        assert!(patched.contains("alpha.example"));
    }

    #[test]
    fn test_patch_wezterm_config_block_updates_existing_block() {
        let original = r"local wezterm = require 'wezterm'
local config = {}
";
        let file = create_temp_config(original);
        let old_block = generate_ssh_domains_lua(&[], 10_000);
        let new_hosts = vec![SshHost {
            alias: "beta".to_string(),
            hostname: Some("beta.example".to_string()),
            user: Some("dev".to_string()),
            port: Some(2200),
            identity_files: Vec::new(),
        }];
        let new_block = generate_ssh_domains_lua(&new_hosts, 50_000);

        let _ = patch_wezterm_config_block_at(file.path(), &old_block).unwrap();
        let result = patch_wezterm_config_block_at(file.path(), &new_block).unwrap();
        assert!(result.modified);

        let patched = fs::read_to_string(file.path()).unwrap();
        assert_eq!(patched.matches(WA_BEGIN_MARKER).count(), 1);
        assert!(patched.contains("beta.example"));
        assert!(patched.contains("port = 2200"));
    }

    #[test]
    fn test_patch_wezterm_config_block_is_idempotent() {
        let original = r"local wezterm = require 'wezterm'
local config = {}
";
        let file = create_temp_config(original);
        let hosts = vec![SshHost {
            alias: "gamma".to_string(),
            hostname: Some("gamma.example".to_string()),
            user: None,
            port: None,
            identity_files: Vec::new(),
        }];
        let block = generate_ssh_domains_lua(&hosts, 50_000);

        let _ = patch_wezterm_config_block_at(file.path(), &block).unwrap();
        let result = patch_wezterm_config_block_at(file.path(), &block).unwrap();
        assert!(!result.modified);

        let patched = fs::read_to_string(file.path()).unwrap();
        assert_eq!(patched.matches(WA_BEGIN_MARKER).count(), 1);
        assert!(patched.contains("gamma.example"));
    }

    #[test]
    fn test_patch_fixture_missing_inserts_once() {
        let original = setup_fixture("wezterm_missing.lua");
        let file = create_temp_config(original);

        let result = patch_wezterm_config_at(file.path()).unwrap();

        assert!(result.modified);
        assert!(result.backup_path.is_some());

        let patched = fs::read_to_string(file.path()).unwrap();
        assert_eq!(patched.matches(WA_BEGIN_MARKER).count(), 1);
        assert_eq!(patched.matches(WA_END_MARKER).count(), 1);
    }

    #[test]
    fn test_patch_fixture_present_is_idempotent() {
        let original = setup_fixture("wezterm_present.lua");
        let file = create_temp_config(original);

        let result = patch_wezterm_config_at(file.path()).unwrap();

        assert!(!result.modified);
        assert!(result.backup_path.is_none());

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
        // Status update snippet should be removed
        assert!(!block.contains("update-status"));
        assert!(!block.contains("--from-status"));
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
    fn test_shell_patch_fixture_missing_inserts_once() {
        let original = setup_fixture("shell_missing.bashrc");
        let file = create_temp_config(original);

        let result = patch_shell_rc_at(file.path(), ShellType::Bash).unwrap();

        assert!(result.modified);
        assert!(result.backup_path.is_some());

        let patched = fs::read_to_string(file.path()).unwrap();
        assert_eq!(patched.matches(WA_BEGIN_MARKER_SHELL).count(), 1);
        assert_eq!(patched.matches(WA_END_MARKER_SHELL).count(), 1);
    }

    #[test]
    fn test_shell_patch_fixture_present_is_idempotent() {
        let original = setup_fixture("shell_present.bashrc");
        let file = create_temp_config(original);

        let result = patch_shell_rc_at(file.path(), ShellType::Bash).unwrap();

        assert!(!result.modified);
        assert!(result.backup_path.is_none());

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

    // =========================================================================
    // Setup Wizard Tests
    // =========================================================================

    use crate::environment::{
        ConnectionType, DetectedAgent, DetectedEnvironment, RemoteHost, ShellInfo, SystemInfo,
        WeztermCapabilities, WeztermInfo,
    };
    use crate::patterns::AgentType;
    use chrono::Utc;

    fn make_test_env(
        wezterm_version: Option<&str>,
        shell: Option<&str>,
        osc_133: bool,
        agents: Vec<(AgentType, u64)>,
        remotes: Vec<(&str, ConnectionType)>,
    ) -> DetectedEnvironment {
        DetectedEnvironment {
            wezterm: WeztermInfo {
                version: wezterm_version.map(str::to_string),
                socket_path: wezterm_version
                    .map(|_| std::path::PathBuf::from("/run/user/1000/wezterm-mux")),
                is_running: wezterm_version.is_some(),
                capabilities: WeztermCapabilities::default(),
            },
            shell: ShellInfo {
                shell_path: shell.map(|s| format!("/bin/{s}")),
                shell_type: shell.map(str::to_string),
                version: shell.map(|_| "5.9".to_string()),
                config_file: None,
                osc_133_enabled: osc_133,
            },
            agents: agents
                .into_iter()
                .map(|(at, pid)| DetectedAgent {
                    agent_type: at,
                    pane_id: pid,
                    confidence: 0.7,
                    indicators: vec!["test".into()],
                })
                .collect(),
            remotes: remotes
                .into_iter()
                .map(|(host, ct)| RemoteHost {
                    hostname: host.to_string(),
                    connection_type: ct,
                    pane_ids: vec![1],
                })
                .collect(),
            system: SystemInfo {
                os: "linux".into(),
                arch: "x86_64".into(),
                cpu_count: 8,
                memory_mb: Some(16384),
                load_average: Some(0.5),
                detected_at_epoch_ms: 0,
            },
            detected_at: Utc::now(),
        }
    }

    #[test]
    fn wizard_detect_shows_wezterm() {
        let env = make_test_env(Some("20260101"), Some("zsh"), true, vec![], vec![]);
        let wizard = SetupWizard::new(env);
        let steps = wizard.detect();
        let labels: Vec<&str> = steps.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"WezTerm CLI"));
        assert!(labels.contains(&"Shell"));
        assert!(labels.contains(&"OSC 133"));
        assert!(labels.contains(&"System"));

        let wez = steps.iter().find(|s| s.label == "WezTerm CLI").unwrap();
        assert!(wez.ok);
        assert!(wez.detail.contains("20260101"));
    }

    #[test]
    fn wizard_detect_missing_wezterm() {
        let env = make_test_env(None, Some("bash"), false, vec![], vec![]);
        let wizard = SetupWizard::new(env);
        let steps = wizard.detect();
        let wez = steps.iter().find(|s| s.label == "WezTerm CLI").unwrap();
        assert!(!wez.ok);
        assert!(wez.detail.contains("not found"));
    }

    #[test]
    fn wizard_detect_shows_agents() {
        let env = make_test_env(
            Some("20260101"),
            Some("zsh"),
            false,
            vec![(AgentType::Codex, 0), (AgentType::ClaudeCode, 1)],
            vec![],
        );
        let wizard = SetupWizard::new(env);
        let steps = wizard.detect();
        assert_eq!(steps.iter().filter(|s| s.label == "Agent").count(), 2);
    }

    #[test]
    fn wizard_detect_shows_remotes() {
        let env = make_test_env(
            Some("20260101"),
            Some("zsh"),
            false,
            vec![],
            vec![("prod.example.com", ConnectionType::Ssh)],
        );
        let wizard = SetupWizard::new(env);
        let steps = wizard.detect();
        let remote_steps: Vec<_> = steps.iter().filter(|s| s.label == "Remote").collect();
        assert_eq!(remote_steps.len(), 1);
        assert!(remote_steps[0].detail.contains("prod.example.com"));
    }

    #[test]
    fn wizard_generate_config_applies_autoconfig() {
        let env = make_test_env(
            Some("20260101"),
            Some("zsh"),
            true,
            vec![(AgentType::Codex, 0)],
            vec![],
        );
        let wizard = SetupWizard::new(env);
        let config = wizard.generate_config();
        assert_eq!(config.ingest.poll_interval_ms, 100);
        assert_eq!(config.ingest.min_poll_interval_ms, 25);
        assert!(config.patterns.packs.contains(&"builtin:core".to_string()));
        assert!(config.patterns.packs.contains(&"builtin:codex".to_string()));
    }

    #[test]
    fn wizard_generate_config_strict_for_production() {
        let env = make_test_env(
            Some("20260101"),
            Some("zsh"),
            false,
            vec![],
            vec![("web-prod-01", ConnectionType::Ssh)],
        );
        let wizard = SetupWizard::new(env);
        let config = wizard.generate_config();
        assert!(config.safety.rate_limit_per_pane <= 10);
    }

    #[test]
    fn wizard_finish_skip_produces_no_config() {
        let env = make_test_env(None, None, false, vec![], vec![]);
        let wizard = SetupWizard::new(env);
        let result = wizard.finish(WizardChoice::Skip, false, None).unwrap();
        assert!(result.config.is_none());
        assert!(result.config_path.is_none());
    }

    #[test]
    fn wizard_finish_accept_saves_config() {
        let env = make_test_env(Some("20260101"), Some("zsh"), true, vec![], vec![]);
        let wizard = SetupWizard::new(env);

        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("wa.toml");
        let result = wizard
            .finish(WizardChoice::Accept, false, Some(&config_path))
            .unwrap();

        assert!(result.config.is_some());
        assert_eq!(result.config_path.as_deref(), Some(config_path.as_path()));
        assert!(config_path.exists());

        let saved = fs::read_to_string(&config_path).unwrap();
        assert!(saved.contains("poll_interval_ms"));
    }

    #[test]
    fn wizard_finish_accept_creates_parent_dirs() {
        let env = make_test_env(None, None, false, vec![], vec![]);
        let wizard = SetupWizard::new(env);

        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("sub").join("deep").join("wa.toml");
        let result = wizard
            .finish(WizardChoice::Accept, false, Some(&config_path))
            .unwrap();

        assert!(config_path.exists());
        assert!(result.config.is_some());
    }

    #[test]
    fn wizard_default_config_save_path_not_none() {
        // On CI/test machines, home dir should generally exist
        if dirs::home_dir().is_some() {
            let path = default_config_save_path();
            assert!(path.is_some());
            let p = path.unwrap();
            assert!(p.to_string_lossy().contains("wa.toml"));
        }
    }

    #[test]
    fn wizard_recommendations_populated() {
        let env = make_test_env(
            Some("20260101"),
            Some("zsh"),
            false,
            vec![(AgentType::Codex, 0)],
            vec![("staging", ConnectionType::Ssh)],
        );
        let wizard = SetupWizard::new(env);
        assert!(!wizard.recommendations().is_empty());
    }
}
