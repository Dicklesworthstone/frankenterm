# WezTerm Automata (wa): A Comprehensive Guide to Automating AI Coding Agent Fleets

> **Purpose**: A complete technical guide for building `wezterm_automata` (`wa`), a high-performance Rust CLI tool for orchestrating fleets of AI coding agents (Claude Code, Codex CLI, Gemini CLI) across distributed WezTerm multiplexer sessions.

> **Note**: wa removed the Lua `update-status` hook in v0.2.0 due to performance overhead. Prefer
> `wezterm cli list` polling, user-var signaling, and escape-sequence detection instead.

---

## Table of Contents

1. [Architecture Overview](#1-architecture-overview)
2. [WezTerm Fundamentals for Automation](#2-wezterm-fundamentals-for-automation)
3. [The WezTerm CLI: Your Primary Interface](#3-the-wezterm-cli-your-primary-interface)
4. [Lua API Deep Dive](#4-lua-api-deep-dive)
5. [Building the wa Rust CLI](#5-building-the-wa-rust-cli)
6. [SQLite Storage Architecture](#6-sqlite-storage-architecture)
7. [Pattern Detection Engine](#7-pattern-detection-engine)
8. [Agent Workflow Automation](#8-agent-workflow-automation)
9. [Browser Automation for Auth Flows](#9-browser-automation-for-auth-flows)
10. [Robot Mode: Agent-First Interface](#10-robot-mode-agent-first-interface)
11. [Automated Setup and Configuration](#11-automated-setup-and-configuration)
12. [Integration with External Tools](#12-integration-with-external-tools)
13. [Performance Optimization](#13-performance-optimization)
14. [Safety and Reliability](#14-safety-and-reliability)
15. [Complete Implementation Reference](#15-complete-implementation-reference)

---

## 1. Architecture Overview

### 1.1 The wa Vision

`wezterm_automata` (wa) is designed to be the central nervous system for managing AI coding agent fleets. Unlike brittle sendkeys-based automation that relies on timing and blind faith, wa leverages WezTerm's rich multiplexer protocol to achieve:

- **Perfect observability**: Real-time capture of all terminal output across all panes
- **Deterministic control**: Send input only when the terminal is in the expected state  
- **Cross-domain orchestration**: Manage agents on local and remote machines uniformly
- **Intelligent reaction**: Pattern-based detection triggers automated workflows

### 1.2 System Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              wa (wezterm_automata)                          │
│                                                                             │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐ │
│  │   Watcher   │  │   Pattern   │  │   Action    │  │   Robot Mode API    │ │
│  │   Engine    │  │   Detector  │  │   Executor  │  │   (Agent Interface) │ │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘  └──────────┬──────────┘ │
│         │                │                │                    │            │
│         └────────────────┴────────────────┴────────────────────┘            │
│                                    │                                        │
│                          ┌────────┴────────┐                                │
│                          │  SQLite + FTS5  │                                │
│                          │   (All Output)  │                                │
│                          └────────┬────────┘                                │
└───────────────────────────────────┼─────────────────────────────────────────┘
                                    │
         ┌──────────────────────────┼──────────────────────────┐
         │                          │                          │
         ▼                          ▼                          ▼
┌─────────────────┐      ┌─────────────────┐      ┌─────────────────┐
│  Local WezTerm  │      │   Remote Mux    │      │   Remote Mux    │
│     (macOS)     │      │  (dev-server)   │      │   (staging)     │
│                 │      │                 │      │                 │
│ ┌─────┐ ┌─────┐│      │ ┌─────┐ ┌─────┐│      │ ┌─────┐ ┌─────┐│
│ │ cc  │ │ cod ││      │ │ cc  │ │ gmi ││      │ │ cod │ │ cc  ││
│ └─────┘ └─────┘│      │ └─────┘ └─────┘│      │ └─────┘ └─────┘│
└─────────────────┘      └─────────────────┘      └─────────────────┘
```

### 1.3 Core Design Principles

1. **Never guess, always observe**: Every action is predicated on observable terminal state
2. **Deterministic over probabilistic**: No sleep/delay heuristics; use actual state transitions
3. **Atomic operations**: Each wa operation either fully succeeds or safely fails
4. **Agent-first ergonomics**: Designed for AI agents to use, not just humans
5. **High-performance by default**: SIMD regex, zero-copy where possible, async I/O

---

## 2. WezTerm Fundamentals for Automation

### 2.1 The Multiplexer Model

WezTerm's multiplexer (`wezterm-mux-server`) is the foundation of reliable automation. Understanding its hierarchy is critical:

```
Multiplexer (mux)
├── Workspaces (logical groupings)
│   └── Windows (gui containers)
│       └── Tabs (within a window)
│           └── Panes (actual terminals)
│               └── Scrollback Buffer (captured output)
└── Domains (connection contexts)
    ├── Local Domain (default)
    ├── SSH Domain (multiplexing = "WezTerm")
    └── Unix Domain (socket-based)
```

**Key insight**: When using SSH domains with `multiplexing = "WezTerm"`, the remote `wezterm-mux-server` maintains all state. Your local GUI merely renders it. This is why sessions persist through Mac sleep/reboot.

### 2.2 Domain Types for Remote Automation

```lua
-- SSH Domain with native multiplexing (RECOMMENDED for wa)
config.ssh_domains = {
  {
    name = 'dev-server',
    remote_address = '10.20.30.1',
    username = 'ubuntu',
    multiplexing = 'WezTerm',  -- Critical: enables mux protocol
    assume_shell = 'Posix',
    -- Optional: custom mux server path
    remote_wezterm_path = '/usr/bin/wezterm',
  },
}

-- Unix Domain (local persistent sessions)
config.unix_domains = {
  {
    name = 'local-mux',
    socket_path = '/tmp/wezterm-mux.sock',
  },
}
```

### 2.3 Scrollback and Output Capture

WezTerm maintains a configurable scrollback buffer per pane:

```lua
-- In wezterm.lua
config.scrollback_lines = 10000  -- Increase for better history

-- Programmatic access via Lua
wezterm.on('some-event', function(window, pane)
  local dims = pane:get_dimensions()
  local total_lines = dims.scrollback_rows + dims.viewport_rows
  
  -- Get ALL text (scrollback + viewport)
  local all_text = pane:get_lines_as_text(dims.scrollback_rows)
  
  -- Get with ANSI escapes (for parsing colors)
  local with_escapes = pane:get_lines_as_escapes(dims.scrollback_rows)
  
  -- Get logical lines (unwrapped)
  local logical = pane:get_logical_lines_as_text(dims.scrollback_rows)
end)
```

### 2.4 Pane Dimensions and Cursor Position

```lua
local dims = pane:get_dimensions()
-- dims.cols: terminal width
-- dims.rows: terminal height  
-- dims.viewport_rows: visible rows
-- dims.scrollback_rows: rows in scrollback
-- dims.physical_top: scroll position

local cursor = pane:get_cursor_position()
-- cursor.x: column (0-indexed)
-- cursor.y: row (0-indexed, viewport-relative)
```

### 2.5 Pane Identity: pane_id vs pane_uuid

WezTerm assigns a numeric `pane_id` per mux session. That value is perfect for
immediate CLI actions (get-text, send-text), but it is not a stable long-lived
identity. Pane ids can be recycled after a pane closes, and they do not survive
daemon restarts or a new mux session.

wa therefore assigns a stable `pane_uuid` at discovery time and persists it:

- Assignment: when a pane is first discovered, wa generates a `pane_uuid` from
  the pane's domain, `pane_id`, first-seen timestamp, and random entropy.
- Stability: the `pane_uuid` never changes while that pane exists, even if the
  pane title, cwd, tab, or window changes.
- Recreation: if a pane is closed and a new pane appears (even if it reuses the
  same numeric `pane_id`), a new `pane_uuid` is generated.
- Persistence: `pane_uuid` is stored in the `panes` table and surfaced in
  `wa status` / `wa robot state` once the watcher has observed the pane.

Identity vs generation:

- `pane_uuid` is the long-lived identity for a pane instance.
- A separate fingerprint (domain + title + cwd + optional initial content hash)
  tracks "generation" changes when the context shifts. Generation increments
  do not change `pane_uuid`.

Observe/act separation:

- `pane_uuid` assignment is passive. wa does not write to panes or use OSC
  handshakes for identity in the current design.
- If a future handshake is added, it must be explicit and rate-limited to avoid
  any "spam" writes in observed panes.

Debugging guidance:

- Use `wa status` or `wa robot state` to confirm the `pane_uuid` associated
  with a pane.
- If identity looks wrong, verify domain/title/cwd and recent pane lifecycle
  changes before taking action.
- When in doubt, treat the pane as new and allow a fresh `pane_uuid` to be
  assigned after rediscovery.

---

## 3. The WezTerm CLI: Your Primary Interface

The `wezterm cli` commands are the primary mechanism for wa to interact with WezTerm. This is more reliable than IPC or Lua callbacks for external tools.

### 3.1 Essential CLI Commands

```bash
# List all panes with full metadata (JSON output)
wezterm cli list --format json

# Output structure:
# [
#   {
#     "window_id": 0,
#     "tab_id": 0,
#     "pane_id": 0,
#     "workspace": "default",
#     "size": { "rows": 24, "cols": 80 },
#     "title": "zsh",
#     "cwd": "file://hostname/path/to/dir"
#   }
# ]

# Get text from a specific pane
wezterm cli get-text --pane-id 3

# Get text with ANSI escapes (for color parsing)
wezterm cli get-text --pane-id 3 --escapes

# Send text to a pane (like paste)
wezterm cli send-text --pane-id 3 "echo hello"

# Send text without bracketed paste mode
wezterm cli send-text --pane-id 3 --no-paste "ls -la\n"

# Spawn a new pane/tab in a domain
wezterm cli spawn --domain-name dev-server --cwd /data/projects

# Split existing pane
wezterm cli split-pane --pane-id 3 --horizontal

# Activate a specific pane
wezterm cli activate-pane --pane-id 3

# Get pane in a direction from current
wezterm cli get-pane-direction --pane-id 3 Right
```

### 3.2 Targeting Panes Across Domains

The `--pane-id` uniquely identifies a pane across all domains. For remote operations:

```bash
# List panes from remote mux
WEZTERM_UNIX_SOCKET=/path/to/remote/sock wezterm cli list

# Or use environment variable from within a wezterm pane
echo $WEZTERM_PANE  # Current pane ID

# Spawn in specific domain
wezterm cli spawn --domain-name staging --new-window \
  -- bash -c "cd /var/www && claude-code"
```

### 3.3 JSON Output Parsing Strategy

wa should always use `--format json` and parse structured output:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct PaneInfo {
    pub window_id: u64,
    pub tab_id: u64,
    pub pane_id: u64,
    pub workspace: String,
    pub size: PaneSize,
    pub title: String,
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PaneSize {
    pub rows: u32,
    pub cols: u32,
}

pub async fn list_panes() -> Result<Vec<PaneInfo>> {
    let output = Command::new("wezterm")
        .args(["cli", "list", "--format", "json"])
        .output()
        .await?;
    
    let panes: Vec<PaneInfo> = serde_json::from_slice(&output.stdout)?;
    Ok(panes)
}
```

---

## 4. Lua API Deep Dive

While wa primarily uses the CLI, understanding Lua enables sophisticated integrations.

### 4.1 Event System

WezTerm fires events that Lua code can handle. Key events for automation:

```lua
local wezterm = require 'wezterm'

-- Called when GUI starts (not mux-only)
wezterm.on('gui-startup', function(cmd)
  -- Spawn initial windows/tabs
end)

-- Called when mux starts (including headless)
wezterm.on('mux-startup', function()
  -- Initialize mux-level state
end)

-- DEPRECATED in wa v0.2.0: update-status hook removed for performance.
-- Use CLI polling + user-var signaling instead. Shown here for reference only.
-- wezterm.on('update-status', function(window, pane)
--   -- Update status bar, check conditions
-- end)

-- Called when bell rings in any pane
wezterm.on('bell', function(window, pane)
  -- Could be used to detect certain events
end)

-- Called when user vars change (powerful for signaling)
wezterm.on('user-var-changed', function(window, pane, name, value)
  if name == 'wa-signal' then
    local cmd = wezterm.json_parse(value)
    -- Handle wa commands from within the terminal
  end
end)
```

### 4.2 User Variables for Bidirectional Communication

User variables allow programs inside a pane to communicate with wezterm.lua:

```bash
# From within a pane (e.g., in a script)
# Set user var 'status' to 'ready'
printf "\033]1337;SetUserVar=%s=%s\007" status $(echo -n ready | base64)

# Complex data via JSON
printf "\033]1337;SetUserVar=%s=%s\007" wa-data \
  $(echo -n '{"event":"compaction","agent":"claude-code"}' | base64)
```

```lua
-- In wezterm.lua
wezterm.on('user-var-changed', function(window, pane, name, value)
  if name == 'wa-data' then
    local data = wezterm.json_parse(value)
    if data.event == 'compaction' then
      -- Trigger wa notification
      wezterm.background_child_process {
        'wa', 'notify', '--event', 'compaction', '--pane', tostring(pane:pane_id())
      }
    end
  end
end)

-- Read all user vars
local vars = pane:get_user_vars()
-- vars.status, vars['wa-data'], etc.
```

### 4.3 Pane Methods for Output Capture

```lua
-- Get text from specific region
local text = pane:get_text_from_region(
  start_row,  -- 0-indexed from top of scrollback
  start_col,
  end_row,
  end_col
)

-- Get semantic zones (if shell integration enabled)
local zones = pane:get_semantic_zones()
for _, zone in ipairs(zones) do
  -- zone.start_x, zone.start_y, zone.end_x, zone.end_y
  -- zone.semantic_type: "Output", "Input", "Prompt"
  local zone_text = pane:get_text_from_semantic_zone(zone)
end

-- Check if alt screen active (vim, less, etc.)
if pane:is_alt_screen_active() then
  -- Handle differently
end
```

### 4.4 Injecting Output (Display-Only)

```lua
-- Inject text into pane's display (does NOT go to the shell)
pane:inject_output("\\x1b[31mwa: Pattern detected!\\x1b[0m\\n")
```

### 4.5 Sending Input

```lua
-- Send text as if typed
pane:send_text("ls -la\n")

-- Send as paste (triggers bracketed paste if enabled)
pane:send_paste("multi\nline\ntext")

-- Via action (more flexible)
window:perform_action(
  wezterm.action.SendString("echo hello\n"),
  pane
)
```

---

## 5. Building the wa Rust CLI

### 5.1 Project Structure

```
wezterm_automata/
├── Cargo.toml
├── src/
│   ├── main.rs
│   ├── cli/
│   │   ├── mod.rs
│   │   ├── robot.rs          # Agent-first interface
│   │   ├── watch.rs          # Continuous monitoring
│   │   ├── setup.rs          # Automated configuration
│   │   └── workflows.rs      # Predefined workflows
│   ├── wezterm/
│   │   ├── mod.rs
│   │   ├── client.rs         # wezterm cli wrapper
│   │   ├── pane.rs           # Pane operations
│   │   └── domain.rs         # Domain management
│   ├── storage/
│   │   ├── mod.rs
│   │   ├── schema.rs         # SQLite schema
│   │   ├── fts.rs            # Full-text search
│   │   └── queries.rs        # Optimized queries
│   ├── patterns/
│   │   ├── mod.rs
│   │   ├── detector.rs       # Pattern matching engine
│   │   ├── agents/
│   │   │   ├── mod.rs
│   │   │   ├── claude_code.rs
│   │   │   ├── codex.rs
│   │   │   └── gemini.rs
│   │   └── actions.rs        # Triggered actions
│   ├── browser/
│   │   ├── mod.rs
│   │   └── auth.rs           # Playwright auth flows
│   └── config/
│       ├── mod.rs
│       └── accounts.rs       # Account rotation
├── skills/
│   ├── AGENTS.md
│   └── workflows/
│       ├── handle_usage_limits.md
│       └── handle_compaction.md
└── tests/
```

### 5.2 Core Dependencies (Cargo.toml)

```toml
[package]
name = "wezterm_automata"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "wa"
path = "src/main.rs"

[dependencies]
# Async runtime
tokio = { version = "1", features = ["full"] }

# CLI framework
clap = { version = "4", features = ["derive", "env"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Database
rusqlite = { version = "0.32", features = ["bundled", "fts5"] }

# High-performance regex
regex = "1"
aho-corasick = "1"  # Multi-pattern matching
# Optional: hyperscan for extreme performance
# hyperscan = { version = "0.3", optional = true }

# Process execution
which = "6"

# Error handling
anyhow = "1"
thiserror = "1"

# Logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Browser automation
playwright = "0.0.20"

# Time handling
chrono = { version = "0.4", features = ["serde"] }

# Configuration
directories = "5"
toml = "0.8"

# SSH config parsing
ssh2-config = "0.2"

[features]
default = []
simd = ["aho-corasick/std"]  # Enable SIMD acceleration
hyperscan = ["dep:hyperscan"]
```

### 5.3 Main Entry Point

```rust
// src/main.rs
use clap::{Parser, Subcommand};
use anyhow::Result;

mod cli;
mod wezterm;
mod storage;
mod patterns;
mod browser;
mod config;

#[derive(Parser)]
#[command(name = "wa")]
#[command(about = "WezTerm Automata - AI Coding Agent Fleet Manager")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    
    /// Output format for robot mode
    #[arg(long, global = true, default_value = "auto")]
    format: OutputFormat,
}

#[derive(Subcommand)]
enum Commands {
    /// Watch all panes and log output (daemon mode)
    Watch(cli::watch::WatchArgs),
    
    /// Robot mode for agent interaction
    Robot(cli::robot::RobotArgs),
    
    /// Execute a predefined workflow
    Workflow(cli::workflows::WorkflowArgs),
    
    /// Setup WezTerm on local/remote machines
    Setup(cli::setup::SetupArgs),
    
    /// Query stored terminal history
    Query(cli::QueryArgs),
    
    /// Get current state of all panes
    Status(cli::StatusArgs),
    
    /// Send command to a pane
    Send(cli::SendArgs),
}

#[derive(Clone, Debug, Default, clap::ValueEnum)]
enum OutputFormat {
    #[default]
    Auto,
    Json,
    Markdown,
    Plain,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("wa=info".parse()?)
        )
        .init();

    let cli = Cli::parse();
    
    // Quick-start mode when no args
    if cli.command.is_none() {
        return cli::quick_start::run(&cli.format).await;
    }
    
    match cli.command.unwrap() {
        Commands::Watch(args) => cli::watch::run(args).await,
        Commands::Robot(args) => cli::robot::run(args, &cli.format).await,
        Commands::Workflow(args) => cli::workflows::run(args).await,
        Commands::Setup(args) => cli::setup::run(args).await,
        Commands::Query(args) => cli::query::run(args, &cli.format).await,
        Commands::Status(args) => cli::status::run(args, &cli.format).await,
        Commands::Send(args) => cli::send::run(args).await,
    }
}
```

### 5.4 WezTerm Client Wrapper

```rust
// src/wezterm/client.rs
use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Stdio;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct WeztermClient {
    /// Path to wezterm binary
    wezterm_path: String,
    /// Optional Unix socket for specific mux
    socket: Option<String>,
}

impl WeztermClient {
    pub fn new() -> Result<Self> {
        let wezterm_path = which::which("wezterm")
            .context("wezterm not found in PATH")?
            .to_string_lossy()
            .to_string();
        
        Ok(Self {
            wezterm_path,
            socket: std::env::var("WEZTERM_UNIX_SOCKET").ok(),
        })
    }
    
    pub fn with_socket(mut self, socket: impl Into<String>) -> Self {
        self.socket = Some(socket.into());
        self
    }
    
    async fn run_cli(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut cmd = Command::new(&self.wezterm_path);
        cmd.args(["cli"]);
        cmd.args(args);
        
        if let Some(ref socket) = self.socket {
            cmd.env("WEZTERM_UNIX_SOCKET", socket);
        }
        
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        
        let output = cmd.output().await?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("wezterm cli failed: {}", stderr);
        }
        
        Ok(output.stdout)
    }
    
    pub async fn list_panes(&self) -> Result<Vec<PaneInfo>> {
        let output = self.run_cli(&["list", "--format", "json"]).await?;
        let panes: Vec<PaneInfo> = serde_json::from_slice(&output)?;
        Ok(panes)
    }
    
    pub async fn get_text(&self, pane_id: u64, with_escapes: bool) -> Result<String> {
        let mut args = vec!["get-text", "--pane-id", &pane_id.to_string()];
        if with_escapes {
            args.push("--escapes");
        }
        let output = self.run_cli(&args).await?;
        Ok(String::from_utf8_lossy(&output).to_string())
    }
    
    pub async fn send_text(&self, pane_id: u64, text: &str, no_paste: bool) -> Result<()> {
        let pane_str = pane_id.to_string();
        let mut args = vec!["send-text", "--pane-id", &pane_str];
        if no_paste {
            args.push("--no-paste");
        }
        args.push(text);
        self.run_cli(&args).await?;
        Ok(())
    }
    
    pub async fn spawn(
        &self,
        domain: Option<&str>,
        cwd: Option<&str>,
        command: Option<&[&str]>,
    ) -> Result<u64> {
        let mut args = vec!["spawn"];
        
        if let Some(d) = domain {
            args.extend(["--domain-name", d]);
        }
        if let Some(c) = cwd {
            args.extend(["--cwd", c]);
        }
        
        if let Some(cmd) = command {
            args.push("--");
            args.extend(cmd.iter().copied());
        }
        
        let output = self.run_cli(&args).await?;
        let pane_id: u64 = String::from_utf8_lossy(&output)
            .trim()
            .parse()
            .context("Failed to parse pane_id from spawn")?;
        
        Ok(pane_id)
    }
    
    pub async fn activate_pane(&self, pane_id: u64) -> Result<()> {
        let pane_str = pane_id.to_string();
        self.run_cli(&["activate-pane", "--pane-id", &pane_str]).await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneInfo {
    pub window_id: u64,
    pub tab_id: u64,
    pub pane_id: u64,
    pub workspace: String,
    pub size: PaneSize,
    pub title: String,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneSize {
    pub rows: u32,
    pub cols: u32,
}
```

---

## 6. SQLite Storage Architecture

### 6.1 Schema Design

```rust
// src/storage/schema.rs
pub const SCHEMA: &str = r#"
-- Main pane registry
CREATE TABLE IF NOT EXISTS panes (
    pane_id INTEGER PRIMARY KEY,
    window_id INTEGER NOT NULL,
    tab_id INTEGER NOT NULL,
    domain_name TEXT NOT NULL,
    workspace TEXT NOT NULL,
    title TEXT,
    cwd TEXT,
    created_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    agent_type TEXT,  -- 'claude_code', 'codex', 'gemini', NULL
    session_id TEXT   -- For correlating with cass
);

-- Full terminal output log
CREATE TABLE IF NOT EXISTS output_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
    captured_at TEXT NOT NULL,
    content TEXT NOT NULL,
    content_hash TEXT NOT NULL,  -- Dedup identical captures
    line_start INTEGER NOT NULL,
    line_end INTEGER NOT NULL,
    UNIQUE(pane_id, content_hash)
);

-- FTS5 virtual table for full-text search
CREATE VIRTUAL TABLE IF NOT EXISTS output_fts USING fts5(
    content,
    pane_id UNINDEXED,
    captured_at UNINDEXED,
    content='output_log',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 1'
);

-- Triggers to keep FTS in sync
CREATE TRIGGER IF NOT EXISTS output_log_ai AFTER INSERT ON output_log BEGIN
    INSERT INTO output_fts(rowid, content, pane_id, captured_at)
    VALUES (new.id, new.content, new.pane_id, new.captured_at);
END;

CREATE TRIGGER IF NOT EXISTS output_log_ad AFTER DELETE ON output_log BEGIN
    INSERT INTO output_fts(output_fts, rowid, content, pane_id, captured_at)
    VALUES ('delete', old.id, old.content, old.pane_id, old.captured_at);
END;

-- Detected events
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
    event_type TEXT NOT NULL,  -- 'usage_limit', 'compaction', 'error', etc.
    agent_type TEXT NOT NULL,
    detected_at TEXT NOT NULL,
    raw_match TEXT,            -- The matched text
    metadata TEXT,             -- JSON with extracted data
    handled_at TEXT,           -- When wa responded
    action_taken TEXT          -- What wa did
);

-- Agent session tracking (correlates with cass)
CREATE TABLE IF NOT EXISTS agent_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
    agent_type TEXT NOT NULL,
    session_id TEXT,           -- Codex session ID, etc.
    started_at TEXT NOT NULL,
    ended_at TEXT,
    total_tokens INTEGER,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cached_tokens INTEGER,
    reasoning_tokens INTEGER
);

-- Account tracking (for usage limit rotation)
CREATE TABLE IF NOT EXISTS accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    service TEXT NOT NULL,     -- 'openai', 'anthropic', 'google'
    account_name TEXT NOT NULL,
    email TEXT,
    usage_resets_at TEXT,
    usage_percent_remaining REAL,
    last_checked_at TEXT,
    credentials_path TEXT,
    UNIQUE(service, account_name)
);

-- Indices for common queries
CREATE INDEX IF NOT EXISTS idx_output_log_pane_time 
    ON output_log(pane_id, captured_at);
CREATE INDEX IF NOT EXISTS idx_events_pane_type 
    ON events(pane_id, event_type);
CREATE INDEX IF NOT EXISTS idx_events_unhandled 
    ON events(handled_at) WHERE handled_at IS NULL;
"#;
```

### 6.2 Storage Implementation

```rust
// src/storage/mod.rs
use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::PathBuf;
use sha2::{Sha256, Digest};

pub struct Storage {
    conn: Connection,
}

impl Storage {
    pub fn open(path: Option<PathBuf>) -> Result<Self> {
        let path = path.unwrap_or_else(|| {
            let dirs = directories::ProjectDirs::from("", "", "wa")
                .expect("Could not determine data directory");
            dirs.data_dir().join("wa.db")
        });
        
        std::fs::create_dir_all(path.parent().unwrap())?;
        
        let conn = Connection::open(&path)?;
        conn.execute_batch(super::schema::SCHEMA)?;
        
        // Enable WAL mode for better concurrent access
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        
        Ok(Self { conn })
    }
    
    pub fn record_output(&self, pane_id: u64, content: &str) -> Result<bool> {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        
        let now = chrono::Utc::now().to_rfc3339();
        
        // Try insert, will fail on duplicate hash (dedup)
        let result = self.conn.execute(
            "INSERT OR IGNORE INTO output_log 
             (pane_id, captured_at, content, content_hash, line_start, line_end)
             VALUES (?1, ?2, ?3, ?4, 0, 0)",
            params![pane_id, now, content, hash],
        )?;
        
        Ok(result > 0)  // True if new content was inserted
    }
    
    pub fn search_output(&self, query: &str, pane_id: Option<u64>) -> Result<Vec<SearchResult>> {
        let sql = if pane_id.is_some() {
            "SELECT pane_id, captured_at, snippet(output_fts, 0, '>>>', '<<<', '...', 64)
             FROM output_fts
             WHERE output_fts MATCH ?1 AND pane_id = ?2
             ORDER BY rank
             LIMIT 100"
        } else {
            "SELECT pane_id, captured_at, snippet(output_fts, 0, '>>>', '<<<', '...', 64)
             FROM output_fts
             WHERE output_fts MATCH ?1
             ORDER BY rank
             LIMIT 100"
        };
        
        let mut stmt = self.conn.prepare(sql)?;
        
        let rows = if let Some(pid) = pane_id {
            stmt.query_map(params![query, pid], |row| {
                Ok(SearchResult {
                    pane_id: row.get(0)?,
                    captured_at: row.get(1)?,
                    snippet: row.get(2)?,
                })
            })?
        } else {
            stmt.query_map(params![query], |row| {
                Ok(SearchResult {
                    pane_id: row.get(0)?,
                    captured_at: row.get(1)?,
                    snippet: row.get(2)?,
                })
            })?
        };
        
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
    
    pub fn record_event(
        &self,
        pane_id: u64,
        event_type: &str,
        agent_type: &str,
        raw_match: &str,
        metadata: Option<&str>,
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        
        self.conn.execute(
            "INSERT INTO events (pane_id, event_type, agent_type, detected_at, raw_match, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![pane_id, event_type, agent_type, now, raw_match, metadata],
        )?;
        
        Ok(self.conn.last_insert_rowid())
    }
    
    pub fn get_unhandled_events(&self) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, pane_id, event_type, agent_type, detected_at, raw_match, metadata
             FROM events
             WHERE handled_at IS NULL
             ORDER BY detected_at ASC"
        )?;
        
        let rows = stmt.query_map([], |row| {
            Ok(Event {
                id: row.get(0)?,
                pane_id: row.get(1)?,
                event_type: row.get(2)?,
                agent_type: row.get(3)?,
                detected_at: row.get(4)?,
                raw_match: row.get(5)?,
                metadata: row.get(6)?,
            })
        })?;
        
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
    
    pub fn mark_event_handled(&self, event_id: i64, action_taken: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE events SET handled_at = ?1, action_taken = ?2 WHERE id = ?3",
            params![now, action_taken, event_id],
        )?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct SearchResult {
    pub pane_id: u64,
    pub captured_at: String,
    pub snippet: String,
}

#[derive(Debug)]
pub struct Event {
    pub id: i64,
    pub pane_id: u64,
    pub event_type: String,
    pub agent_type: String,
    pub detected_at: String,
    pub raw_match: String,
    pub metadata: Option<String>,
}
```

---

## 7. Pattern Detection Engine

### 7.1 Multi-Pattern Matcher Design

```rust
// src/patterns/detector.rs
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use regex::Regex;
use std::collections::HashMap;

/// A detected pattern with extracted information
#[derive(Debug, Clone)]
pub struct Detection {
    pub pattern_id: &'static str,
    pub agent: AgentType,
    pub event: EventType,
    pub raw_match: String,
    pub position: usize,
    pub extracted: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentType {
    ClaudeCode,
    Codex,
    Gemini,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    UsageLimitWarning,
    UsageLimitReached,
    Compaction,
    SessionEnd,
    AuthRequired,
    Error,
}

/// High-performance multi-pattern detector
pub struct PatternDetector {
    // Fast literal matcher for initial scanning
    literal_matcher: AhoCorasick,
    // Map from literal index to pattern definition
    literal_to_pattern: Vec<PatternDef>,
    // Regex patterns for complex matches
    regex_patterns: Vec<(Regex, PatternDef)>,
}

#[derive(Clone)]
struct PatternDef {
    id: &'static str,
    agent: AgentType,
    event: EventType,
    // Optional regex for extraction (run after literal match)
    extractor: Option<Regex>,
}

impl PatternDetector {
    pub fn new() -> Self {
        let mut literals = Vec::new();
        let mut literal_to_pattern = Vec::new();
        let mut regex_patterns = Vec::new();
        
        // Claude Code patterns
        Self::add_claude_code_patterns(&mut literals, &mut literal_to_pattern, &mut regex_patterns);
        
        // Codex patterns
        Self::add_codex_patterns(&mut literals, &mut literal_to_pattern, &mut regex_patterns);
        
        // Gemini CLI patterns
        Self::add_gemini_patterns(&mut literals, &mut literal_to_pattern, &mut regex_patterns);
        
        let literal_matcher = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostFirst)
            .build(&literals)
            .expect("Failed to build Aho-Corasick automaton");
        
        Self {
            literal_matcher,
            literal_to_pattern,
            regex_patterns,
        }
    }
    
    fn add_claude_code_patterns(
        literals: &mut Vec<&'static str>,
        literal_to_pattern: &mut Vec<PatternDef>,
        _regex_patterns: &mut Vec<(Regex, PatternDef)>,
    ) {
        // Usage limit reached
        literals.push("You've hit your limit");
        literal_to_pattern.push(PatternDef {
            id: "cc_usage_limit",
            agent: AgentType::ClaudeCode,
            event: EventType::UsageLimitReached,
            extractor: Some(Regex::new(r"resets\s+(\d+[ap]m)\s+\(([^)]+)\)").unwrap()),
        });
        
        // Compaction indicator
        literals.push("Conversation compacted");
        literal_to_pattern.push(PatternDef {
            id: "cc_compaction",
            agent: AgentType::ClaudeCode,
            event: EventType::Compaction,
            extractor: None,
        });
        
        // Claude Code banner (session start)
        literals.push("Claude Code v");
        literal_to_pattern.push(PatternDef {
            id: "cc_session_start",
            agent: AgentType::ClaudeCode,
            event: EventType::SessionEnd,  // Actually start, but we'll handle
            extractor: Some(Regex::new(r"Claude Code v(\d+\.\d+\.\d+)").unwrap()),
        });
    }
    
    fn add_codex_patterns(
        literals: &mut Vec<&'static str>,
        literal_to_pattern: &mut Vec<PatternDef>,
        _regex_patterns: &mut Vec<(Regex, PatternDef)>,
    ) {
        // Usage warnings
        literals.push("you have less than 25%");
        literal_to_pattern.push(PatternDef {
            id: "codex_usage_25",
            agent: AgentType::Codex,
            event: EventType::UsageLimitWarning,
            extractor: None,
        });
        
        literals.push("you have less than 10%");
        literal_to_pattern.push(PatternDef {
            id: "codex_usage_10",
            agent: AgentType::Codex,
            event: EventType::UsageLimitWarning,
            extractor: None,
        });
        
        literals.push("you have less than 5%");
        literal_to_pattern.push(PatternDef {
            id: "codex_usage_5",
            agent: AgentType::Codex,
            event: EventType::UsageLimitWarning,
            extractor: None,
        });
        
        // Usage limit reached
        literals.push("You've hit your usage limit");
        literal_to_pattern.push(PatternDef {
            id: "codex_usage_limit",
            agent: AgentType::Codex,
            event: EventType::UsageLimitReached,
            extractor: Some(Regex::new(r"try again at ([^.]+)").unwrap()),
        });
        
        // Session end with token stats
        literals.push("Token usage:");
        literal_to_pattern.push(PatternDef {
            id: "codex_session_end",
            agent: AgentType::Codex,
            event: EventType::SessionEnd,
            extractor: Some(Regex::new(
                r"total=(\d+)\s+input=(\d+)\s+\(\+\s*(\d+)\s+cached\)\s+output=(\d+)(?:\s+\(reasoning\s+(\d+)\))?"
            ).unwrap()),
        });
        
        // Resume session ID
        literals.push("To continue this session, run codex resume");
        literal_to_pattern.push(PatternDef {
            id: "codex_resume_hint",
            agent: AgentType::Codex,
            event: EventType::SessionEnd,
            extractor: Some(Regex::new(r"resume\s+([0-9a-f-]+)").unwrap()),
        });
        
        // Device auth prompt
        literals.push("Enter this one-time code");
        literal_to_pattern.push(PatternDef {
            id: "codex_device_auth",
            agent: AgentType::Codex,
            event: EventType::AuthRequired,
            extractor: Some(Regex::new(r"([A-Z0-9]{4}-[A-Z0-9]{5})").unwrap()),
        });
    }
    
    fn add_gemini_patterns(
        literals: &mut Vec<&'static str>,
        literal_to_pattern: &mut Vec<PatternDef>,
        _regex_patterns: &mut Vec<(Regex, PatternDef)>,
    ) {
        // Usage limit
        literals.push("Usage limit reached for all Pro models");
        literal_to_pattern.push(PatternDef {
            id: "gemini_usage_limit",
            agent: AgentType::Gemini,
            event: EventType::UsageLimitReached,
            extractor: None,
        });
        
        // Model indicator
        literals.push("Responding with gemini-");
        literal_to_pattern.push(PatternDef {
            id: "gemini_model",
            agent: AgentType::Gemini,
            event: EventType::SessionEnd,  // Used to track which model
            extractor: Some(Regex::new(r"Responding with (gemini-[^\s]+)").unwrap()),
        });
    }
    
    /// Scan text for all pattern matches
    pub fn detect(&self, text: &str) -> Vec<Detection> {
        let mut detections = Vec::new();
        
        // Fast path: Aho-Corasick for literal patterns
        for mat in self.literal_matcher.find_iter(text) {
            let pattern_def = &self.literal_to_pattern[mat.pattern().as_usize()];
            
            // Get surrounding context for extraction
            let context_start = mat.start().saturating_sub(100);
            let context_end = (mat.end() + 200).min(text.len());
            let context = &text[context_start..context_end];
            
            let mut extracted = HashMap::new();
            
            if let Some(ref extractor) = pattern_def.extractor {
                if let Some(caps) = extractor.captures(context) {
                    for (i, name) in extractor.capture_names().enumerate() {
                        if let Some(n) = name {
                            if let Some(m) = caps.get(i) {
                                extracted.insert(n.to_string(), m.as_str().to_string());
                            }
                        } else if i > 0 {
                            if let Some(m) = caps.get(i) {
                                extracted.insert(format!("group_{}", i), m.as_str().to_string());
                            }
                        }
                    }
                }
            }
            
            detections.push(Detection {
                pattern_id: pattern_def.id,
                agent: pattern_def.agent,
                event: pattern_def.event,
                raw_match: text[mat.start()..mat.end().min(mat.start() + 100)].to_string(),
                position: mat.start(),
                extracted,
            });
        }
        
        // Slow path: regex patterns (for complex patterns not suitable for literals)
        for (regex, pattern_def) in &self.regex_patterns {
            for caps in regex.captures_iter(text) {
                let full_match = caps.get(0).unwrap();
                let mut extracted = HashMap::new();
                
                for (i, name) in regex.capture_names().enumerate() {
                    if let Some(n) = name {
                        if let Some(m) = caps.get(i) {
                            extracted.insert(n.to_string(), m.as_str().to_string());
                        }
                    }
                }
                
                detections.push(Detection {
                    pattern_id: pattern_def.id,
                    agent: pattern_def.agent,
                    event: pattern_def.event,
                    raw_match: full_match.as_str().to_string(),
                    position: full_match.start(),
                    extracted,
                });
            }
        }
        
        // Sort by position for chronological processing
        detections.sort_by_key(|d| d.position);
        
        detections
    }
}
```

### 7.2 Agent-Specific Parsers

```rust
// src/patterns/agents/codex.rs
use super::super::{Detection, AgentType, EventType};
use regex::Regex;
use lazy_static::lazy_static;

lazy_static! {
    static ref TOKEN_USAGE_RE: Regex = Regex::new(
        r"Token usage:\s*total=(\d+)\s+input=(\d+)\s+\(\+\s*([0-9,]+)\s+cached\)\s+output=(\d+)(?:\s+\(reasoning\s+(\d+)\))?"
    ).unwrap();
    
    static ref SESSION_ID_RE: Regex = Regex::new(
        r"codex resume ([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})"
    ).unwrap();
    
    static ref DEVICE_CODE_RE: Regex = Regex::new(
        r"([A-Z0-9]{4}-[A-Z0-9]{5})"
    ).unwrap();
}

#[derive(Debug, Clone)]
pub struct CodexTokenUsage {
    pub total: u64,
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub reasoning: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct CodexSessionEnd {
    pub session_id: String,
    pub token_usage: Option<CodexTokenUsage>,
}

pub fn parse_session_end(text: &str) -> Option<CodexSessionEnd> {
    let session_id = SESSION_ID_RE
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())?;
    
    let token_usage = TOKEN_USAGE_RE.captures(text).map(|caps| {
        CodexTokenUsage {
            total: caps.get(1).unwrap().as_str().parse().unwrap_or(0),
            input: caps.get(2).unwrap().as_str().parse().unwrap_or(0),
            cached: caps.get(3)
                .unwrap()
                .as_str()
                .replace(',', "")
                .parse()
                .unwrap_or(0),
            output: caps.get(4).unwrap().as_str().parse().unwrap_or(0),
            reasoning: caps.get(5).and_then(|m| m.as_str().parse().ok()),
        }
    });
    
    Some(CodexSessionEnd {
        session_id,
        token_usage,
    })
}

pub fn parse_device_code(text: &str) -> Option<String> {
    DEVICE_CODE_RE
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}
```

---

## 8. Agent Workflow Automation

### 8.1 Workflow Engine

```rust
// src/cli/workflows.rs
use anyhow::Result;
use crate::wezterm::WeztermClient;
use crate::storage::Storage;
use crate::patterns::{Detection, EventType, AgentType};

#[derive(Debug, Clone)]
pub struct WorkflowContext {
    pub client: WeztermClient,
    pub storage: Storage,
    pub pane_id: u64,
    pub detection: Detection,
}

pub trait Workflow: Send + Sync {
    fn name(&self) -> &'static str;
    fn handles(&self, detection: &Detection) -> bool;
    async fn execute(&self, ctx: WorkflowContext) -> Result<String>;
}

// Handle usage limits by account rotation
pub struct HandleUsageLimits;

impl Workflow for HandleUsageLimits {
    fn name(&self) -> &'static str {
        "handle_usage_limits"
    }
    
    fn handles(&self, detection: &Detection) -> bool {
        matches!(detection.event, EventType::UsageLimitReached)
    }
    
    async fn execute(&self, ctx: WorkflowContext) -> Result<String> {
        match ctx.detection.agent {
            AgentType::Codex => self.handle_codex(ctx).await,
            AgentType::ClaudeCode => self.handle_claude_code(ctx).await,
            AgentType::Gemini => self.handle_gemini(ctx).await,
        }
    }
}

impl HandleUsageLimits {
    async fn handle_codex(&self, ctx: WorkflowContext) -> Result<String> {
        // 1. Send Ctrl-C twice to exit cleanly
        ctx.client.send_text(ctx.pane_id, "\x03\x03", true).await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        
        // 2. Parse the session ID from the output
        let text = ctx.client.get_text(ctx.pane_id, false).await?;
        let session_id = crate::patterns::agents::codex::parse_session_end(&text)
            .map(|s| s.session_id)
            .ok_or_else(|| anyhow::anyhow!("Could not find session ID"))?;
        
        // 3. Get next available account
        let next_account = ctx.storage.get_next_available_account("openai")?;
        
        // 4. Perform device auth login
        ctx.client.send_text(ctx.pane_id, "cod login --device-auth\n", true).await?;
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        
        // 5. Get the device code
        let text = ctx.client.get_text(ctx.pane_id, false).await?;
        let device_code = crate::patterns::agents::codex::parse_device_code(&text)
            .ok_or_else(|| anyhow::anyhow!("Could not find device code"))?;
        
        // 6. Automate browser login
        crate::browser::auth::complete_openai_device_auth(
            &device_code,
            &next_account,
        ).await?;
        
        // 7. Wait for login confirmation
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        
        // 8. Resume the session
        ctx.client.send_text(
            ctx.pane_id, 
            &format!("cod resume {}\n", session_id),
            true
        ).await?;
        
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        
        // 9. Send continue prompt
        ctx.client.send_text(ctx.pane_id, "proceed.\n", true).await?;
        
        Ok(format!(
            "Rotated to account {} and resumed session {}",
            next_account.account_name,
            session_id
        ))
    }
    
    async fn handle_claude_code(&self, ctx: WorkflowContext) -> Result<String> {
        // Claude Code specific handling
        // Similar pattern but with /login command
        todo!("Implement Claude Code account rotation")
    }
    
    async fn handle_gemini(&self, ctx: WorkflowContext) -> Result<String> {
        // Gemini CLI specific handling
        // Uses /auth command
        todo!("Implement Gemini account rotation")
    }
}

// Handle compaction by refreshing context
pub struct HandleCompaction;

impl Workflow for HandleCompaction {
    fn name(&self) -> &'static str {
        "handle_compaction"
    }
    
    fn handles(&self, detection: &Detection) -> bool {
        matches!(detection.event, EventType::Compaction)
    }
    
    async fn execute(&self, ctx: WorkflowContext) -> Result<String> {
        // Wait a moment for compaction to complete
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        
        // Send context refresh prompt
        let prompt = match ctx.detection.agent {
            AgentType::ClaudeCode => {
                "Reread AGENTS.md so it's still fresh in your mind.\n"
            }
            AgentType::Codex => {
                "Please reread the project context files to refresh your memory.\n"
            }
            AgentType::Gemini => {
                "Please re-examine the key project files.\n"
            }
        };
        
        ctx.client.send_text(ctx.pane_id, prompt, true).await?;
        
        Ok(format!("Sent context refresh prompt for {:?}", ctx.detection.agent))
    }
}
```

### 8.2 Watcher Daemon

```rust
// src/cli/watch.rs
use anyhow::Result;
use clap::Args;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::interval;

#[derive(Args, Debug)]
pub struct WatchArgs {
    /// Polling interval in milliseconds
    #[arg(long, default_value = "500")]
    interval_ms: u64,
    
    /// Only watch specific domains
    #[arg(long)]
    domains: Vec<String>,
    
    /// Auto-handle detected events
    #[arg(long, default_value = "true")]
    auto_handle: bool,
}

pub async fn run(args: WatchArgs) -> Result<()> {
    let client = crate::wezterm::WeztermClient::new()?;
    let storage = crate::storage::Storage::open(None)?;
    let detector = crate::patterns::PatternDetector::new();
    
    let workflows: Vec<Box<dyn crate::cli::workflows::Workflow>> = vec![
        Box::new(crate::cli::workflows::HandleUsageLimits),
        Box::new(crate::cli::workflows::HandleCompaction),
    ];
    
    // Track last seen content hash per pane for change detection
    let mut last_hashes: HashMap<u64, u64> = HashMap::new();
    
    let mut ticker = interval(Duration::from_millis(args.interval_ms));
    
    tracing::info!("Starting wa watcher daemon (interval={}ms)", args.interval_ms);
    
    loop {
        ticker.tick().await;
        
        let panes = match client.list_panes().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to list panes: {}", e);
                continue;
            }
        };
        
        for pane in panes {
            // Filter by domain if specified
            if !args.domains.is_empty() {
                // Would need domain info from pane - TODO
            }
            
            // Get current text
            let text = match client.get_text(pane.pane_id, false).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::debug!("Failed to get text from pane {}: {}", pane.pane_id, e);
                    continue;
                }
            };
            
            // Quick hash check for changes
            let hash = fxhash::hash64(&text);
            if last_hashes.get(&pane.pane_id) == Some(&hash) {
                continue;  // No change
            }
            last_hashes.insert(pane.pane_id, hash);
            
            // Store the new content
            if let Err(e) = storage.record_output(pane.pane_id, &text) {
                tracing::warn!("Failed to store output for pane {}: {}", pane.pane_id, e);
            }
            
            // Detect patterns
            let detections = detector.detect(&text);
            
            for detection in detections {
                tracing::info!(
                    "Detected {:?} in pane {} (agent={:?})",
                    detection.event,
                    pane.pane_id,
                    detection.agent
                );
                
                // Record the event
                let event_id = storage.record_event(
                    pane.pane_id,
                    &format!("{:?}", detection.event),
                    &format!("{:?}", detection.agent),
                    &detection.raw_match,
                    serde_json::to_string(&detection.extracted).ok().as_deref(),
                )?;
                
                // Auto-handle if enabled
                if args.auto_handle {
                    for workflow in &workflows {
                        if workflow.handles(&detection) {
                            let ctx = crate::cli::workflows::WorkflowContext {
                                client: client.clone(),
                                storage: storage.clone(),
                                pane_id: pane.pane_id,
                                detection: detection.clone(),
                            };
                            
                            match workflow.execute(ctx).await {
                                Ok(result) => {
                                    tracing::info!(
                                        "Workflow {} completed: {}",
                                        workflow.name(),
                                        result
                                    );
                                    storage.mark_event_handled(event_id, &result)?;
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "Workflow {} failed: {}",
                                        workflow.name(),
                                        e
                                    );
                                }
                            }
                            break;  // Only one workflow per detection
                        }
                    }
                }
            }
        }
    }
}
```

---

## 9. Browser Automation for Auth Flows

### 9.1 Playwright Integration

```rust
// src/browser/auth.rs
use anyhow::Result;
use playwright::Playwright;

pub struct Account {
    pub service: String,
    pub account_name: String,
    pub email: String,
    pub credentials_path: Option<String>,
}

pub async fn complete_openai_device_auth(
    device_code: &str,
    account: &Account,
) -> Result<()> {
    let playwright = Playwright::initialize().await?;
    playwright.prepare()?;  // Install browsers if needed
    
    let browser = playwright
        .chromium()
        .launcher()
        .headless(true)
        .launch()
        .await?;
    
    let context = browser.context_builder().build().await?;
    let page = context.new_page().await?;
    
    // Navigate to device auth page
    page.goto_builder("https://auth.openai.com/codex/device")
        .goto()
        .await?;
    
    // Wait for email input
    page.wait_for_selector_builder("input[name='email']")
        .wait_for_selector()
        .await?;
    
    // Enter email
    page.fill_builder("input[name='email']", &account.email)
        .fill()
        .await?;
    
    page.click_builder("button[type='submit']")
        .click()
        .await?;
    
    // Wait for password or OTP page
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    
    // Handle password if needed (would need secure credential storage)
    // For now, assume SSO or already logged in
    
    // Wait for device code input
    page.wait_for_selector_builder("input[name='user_code']")
        .timeout(30000.0)
        .wait_for_selector()
        .await?;
    
    // Enter the device code
    page.fill_builder("input[name='user_code']", device_code)
        .fill()
        .await?;
    
    page.click_builder("button[type='submit']")
        .click()
        .await?;
    
    // Wait for confirmation
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    
    // Look for success indicator
    let success = page
        .query_selector("text=Successfully logged in")
        .await?
        .is_some();
    
    browser.close().await?;
    
    if success {
        Ok(())
    } else {
        anyhow::bail!("Device auth did not complete successfully")
    }
}

// Similar functions for Anthropic and Google auth...
```

---

## 10. Robot Mode: Agent-First Interface

### 10.1 Design Philosophy

Robot mode is designed for AI coding agents to interact with wa. Key principles:

1. **Explicit over implicit**: Every command clearly states what it does
2. **Structured output**: JSON for data, Markdown for reports
3. **Token-efficient**: Minimal verbosity, maximum information density
4. **Self-documenting**: Built-in help that agents can parse

### 10.2 Robot Mode Implementation

```rust
// src/cli/robot.rs
use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;

#[derive(Args, Debug)]
pub struct RobotArgs {
    #[command(subcommand)]
    command: RobotCommand,
}

#[derive(Subcommand, Debug)]
pub enum RobotCommand {
    /// Get current state of all panes
    State,
    /// Get text from a specific pane
    GetText {
        pane_id: u64,
        #[arg(long)]
        last_n_lines: Option<usize>,
    },
    /// Send text to a pane
    Send {
        pane_id: u64,
        text: String,
        #[arg(long)]
        no_newline: bool,
    },
    /// Wait for pattern in pane output
    WaitFor {
        pane_id: u64,
        pattern: String,
        #[arg(long, default_value = "30")]
        timeout_secs: u64,
    },
    /// Search history
    Search {
        query: String,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Get recent events
    Events {
        #[arg(long)]
        unhandled_only: bool,
    },
    /// Execute a workflow
    Workflow {
        name: String,
        pane_id: u64,
    },
    /// Quick-start help optimized for agents
    Help,
}

pub async fn run(args: RobotArgs, format: &super::OutputFormat) -> Result<()> {
    let output = match args.command {
        RobotCommand::State => state_command().await?,
        RobotCommand::GetText { pane_id, last_n_lines } => {
            get_text_command(pane_id, last_n_lines).await?
        }
        RobotCommand::Send { pane_id, text, no_newline } => {
            send_command(pane_id, &text, no_newline).await?
        }
        RobotCommand::WaitFor { pane_id, pattern, timeout_secs } => {
            wait_for_command(pane_id, &pattern, timeout_secs).await?
        }
        RobotCommand::Search { query, pane_id } => {
            search_command(&query, pane_id).await?
        }
        RobotCommand::Events { unhandled_only } => {
            events_command(unhandled_only).await?
        }
        RobotCommand::Workflow { name, pane_id } => {
            workflow_command(&name, pane_id).await?
        }
        RobotCommand::Help => help_output(),
    };
    
    match format {
        super::OutputFormat::Json | super::OutputFormat::Auto => {
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        super::OutputFormat::Markdown => {
            println!("{}", output.to_markdown());
        }
        super::OutputFormat::Plain => {
            println!("{}", output.to_plain());
        }
    }
    
    Ok(())
}

#[derive(Serialize)]
struct RobotOutput {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

impl RobotOutput {
    fn success(data: impl Serialize) -> Self {
        Self {
            success: true,
            data: Some(serde_json::to_value(data).unwrap()),
            error: None,
            hint: None,
        }
    }
    
    fn error(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(msg.into()),
            hint: None,
        }
    }
    
    fn to_markdown(&self) -> String {
        if self.success {
            format!("## Success\n\n```json\n{}\n```", 
                serde_json::to_string_pretty(&self.data).unwrap())
        } else {
            format!("## Error\n\n{}", self.error.as_ref().unwrap())
        }
    }
    
    fn to_plain(&self) -> String {
        if self.success {
            serde_json::to_string(&self.data).unwrap()
        } else {
            format!("ERROR: {}", self.error.as_ref().unwrap())
        }
    }
}

async fn state_command() -> Result<RobotOutput> {
    let client = crate::wezterm::WeztermClient::new()?;
    let panes = client.list_panes().await?;
    
    #[derive(Serialize)]
    struct PaneState {
        pane_id: u64,
        window_id: u64,
        tab_id: u64,
        title: String,
        size: String,
        cwd: Option<String>,
    }
    
    let states: Vec<PaneState> = panes.into_iter().map(|p| PaneState {
        pane_id: p.pane_id,
        window_id: p.window_id,
        tab_id: p.tab_id,
        title: p.title,
        size: format!("{}x{}", p.size.cols, p.size.rows),
        cwd: p.cwd,
    }).collect();
    
    Ok(RobotOutput::success(states))
}

fn help_output() -> RobotOutput {
    let help = r#"# wa robot - Agent Interface

## Quick Reference

| Command | Purpose | Example |
|---------|---------|---------|
| state | List all panes | `wa robot state` |
| get-text | Read pane output | `wa robot get-text 3 --last-n-lines 50` |
| send | Send input | `wa robot send 3 "ls -la"` |
| wait-for | Wait for pattern | `wa robot wait-for 3 "done" --timeout-secs 60` |
| search | Search history | `wa robot search "error"` |
| events | List events | `wa robot events --unhandled-only` |
| workflow | Run workflow | `wa robot workflow handle_usage_limits 3` |

## Common Patterns

### Check if agent hit usage limit
```bash
wa robot search "hit your limit" --pane-id 3
```

### Send multi-line input
```bash
wa robot send 3 "cat << 'EOF'
line 1
line 2
EOF"
```

### Wait for prompt then send
```bash
wa robot wait-for 3 ">" && wa robot send 3 "continue"
```

## Output Format

All commands output JSON with structure:
```json
{"success": true, "data": {...}}
{"success": false, "error": "message"}
```
"#;
    
    RobotOutput {
        success: true,
        data: Some(serde_json::json!({ "help": help })),
        error: None,
        hint: Some("Use --format markdown for readable help".into()),
    }
}
```

### 10.3 Quick-Start (No Arguments)

```rust
// src/cli/quick_start.rs
use anyhow::Result;

pub async fn run(format: &super::OutputFormat) -> Result<()> {
    let help = r#"
╔══════════════════════════════════════════════════════════════════════════════╗
║                    wa - WezTerm Automata v0.1.0                              ║
║                    AI Coding Agent Fleet Manager                              ║
╚══════════════════════════════════════════════════════════════════════════════╝

QUICK START
-----------
  wa watch           Start daemon (monitors all panes, auto-handles events)
  wa status          Show current pane states
  wa robot help      Agent-optimized interface documentation

CORE COMMANDS
-------------
  wa watch [--interval-ms 500]     Continuous monitoring daemon
  wa robot <cmd>                   Agent-first JSON interface
  wa query <search-term>           Search all captured output
  wa send <pane-id> <text>         Send input to pane
  wa workflow <name> <pane-id>     Execute predefined workflow

ROBOT MODE (for AI agents)
--------------------------
  wa robot state                   JSON list of all panes
  wa robot get-text <pane-id>      Get pane content
  wa robot send <pane-id> <text>   Send input
  wa robot wait-for <pane-id> <pattern>  Wait for output pattern
  wa robot search <query>          Search history (FTS5)
  wa robot events                  List detected events
  wa robot workflow <name> <pane>  Execute workflow

SETUP
-----
  wa setup local                   Configure local WezTerm
  wa setup remote <host>           Configure remote mux server
  wa setup accounts                Manage service accounts

EXAMPLES
--------
  # Start monitoring all agents
  wa watch --auto-handle

  # Send to specific pane
  wa send 3 "proceed with the implementation"

  # Search for errors across all history
  wa query "error OR failed OR exception"

  # Agent checking status
  wa robot state | jq '.data[] | select(.title | contains("claude"))'

For detailed help: wa <command> --help
Documentation: https://github.com/your-repo/wa
"#;
    
    match format {
        super::OutputFormat::Json => {
            println!("{}", serde_json::json!({
                "name": "wa",
                "version": env!("CARGO_PKG_VERSION"),
                "commands": ["watch", "robot", "query", "send", "workflow", "setup", "status"],
                "robot_commands": ["state", "get-text", "send", "wait-for", "search", "events", "workflow", "help"],
            }));
        }
        _ => {
            println!("{}", help);
        }
    }
    
    Ok(())
}
```

---

## 11. Automated Setup and Configuration

### 11.1 SSH Config Parser

```rust
// src/cli/setup.rs
use anyhow::Result;
use std::path::PathBuf;

pub struct SshHost {
    pub name: String,
    pub hostname: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<PathBuf>,
}

pub fn parse_ssh_config() -> Result<Vec<SshHost>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory"))?;
    let config_path = home.join(".ssh").join("config");
    
    if !config_path.exists() {
        return Ok(Vec::new());
    }
    
    let content = std::fs::read_to_string(&config_path)?;
    let mut hosts = Vec::new();
    let mut current_host: Option<SshHost> = None;
    
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        
        let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
        if parts.len() != 2 {
            continue;
        }
        
        let key = parts[0].to_lowercase();
        let value = parts[1].trim();
        
        match key.as_str() {
            "host" => {
                if let Some(host) = current_host.take() {
                    if !host.name.contains('*') {
                        hosts.push(host);
                    }
                }
                current_host = Some(SshHost {
                    name: value.to_string(),
                    hostname: value.to_string(),
                    user: None,
                    port: None,
                    identity_file: None,
                });
            }
            "hostname" => {
                if let Some(ref mut host) = current_host {
                    host.hostname = value.to_string();
                }
            }
            "user" => {
                if let Some(ref mut host) = current_host {
                    host.user = Some(value.to_string());
                }
            }
            "port" => {
                if let Some(ref mut host) = current_host {
                    host.port = value.parse().ok();
                }
            }
            "identityfile" => {
                if let Some(ref mut host) = current_host {
                    let path = if value.starts_with('~') {
                        home.join(&value[2..])
                    } else {
                        PathBuf::from(value)
                    };
                    host.identity_file = Some(path);
                }
            }
            _ => {}
        }
    }
    
    if let Some(host) = current_host {
        if !host.name.contains('*') {
            hosts.push(host);
        }
    }
    
    Ok(hosts)
}

pub fn generate_wezterm_config(hosts: &[SshHost]) -> String {
    let mut config = String::from(r#"local wezterm = require 'wezterm'
local config = wezterm.config_builder()

-- SSH Domains with WezTerm multiplexing
config.ssh_domains = {
"#);
    
    for host in hosts {
        let user = host.user.as_deref().unwrap_or("$USER");
        config.push_str(&format!(r#"  {{
    name = '{}',
    remote_address = '{}',
    username = '{}',
    multiplexing = 'WezTerm',
    assume_shell = 'Posix',
  }},
"#, host.name, host.hostname, user));
    }
    
    config.push_str(r#"}

return config
"#);
    
    config
}
```

### 11.2 Remote Setup Script

```rust
// Part of setup.rs

pub async fn setup_remote(host: &str) -> Result<()> {
    use tokio::process::Command;
    
    // 1. Check if wezterm is installed
    let check = Command::new("ssh")
        .args([host, "which", "wezterm"])
        .output()
        .await?;
    
    if !check.status.success() {
        tracing::info!("Installing WezTerm on {}", host);
        
        // Install WezTerm
        let install_script = r#"
            curl -fsSL https://apt.fury.io/wez/gpg.key | sudo gpg --dearmor -o /usr/share/keyrings/wezterm-fury.gpg
            echo 'deb [signed-by=/usr/share/keyrings/wezterm-fury.gpg] https://apt.fury.io/wez/ * *' | sudo tee /etc/apt/sources.list.d/wezterm.list
            sudo apt update && sudo apt install -y wezterm
        "#;
        
        Command::new("ssh")
            .args([host, "bash", "-c", install_script])
            .status()
            .await?;
    }
    
    // 2. Create systemd service
    let service_unit = r#"[Unit]
Description=WezTerm Mux Server
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/wezterm-mux-server --daemonize=false
Restart=on-failure
RestartSec=5
Environment=WEZTERM_LOG=warn

[Install]
WantedBy=default.target
"#;
    
    Command::new("ssh")
        .args([
            host,
            "mkdir", "-p", "~/.config/systemd/user",
            "&&",
            "cat", ">", "~/.config/systemd/user/wezterm-mux-server.service",
        ])
        .stdin(std::process::Stdio::piped())
        .spawn()?
        .stdin
        .as_mut()
        .unwrap()
        .write_all(service_unit.as_bytes())
        .await?;
    
    // 3. Enable and start service
    Command::new("ssh")
        .args([
            host,
            "systemctl", "--user", "daemon-reload",
            "&&",
            "systemctl", "--user", "enable", "--now", "wezterm-mux-server",
            "&&",
            "sudo", "loginctl", "enable-linger", "$USER",
        ])
        .status()
        .await?;
    
    // 4. Verify
    let status = Command::new("ssh")
        .args([host, "systemctl", "--user", "status", "wezterm-mux-server"])
        .output()
        .await?;
    
    if status.status.success() {
        tracing::info!("WezTerm mux server running on {}", host);
        Ok(())
    } else {
        anyhow::bail!("Failed to start wezterm-mux-server on {}", host)
    }
}
```

---

## 12. Integration with External Tools

### 12.1 CASS Integration

```rust
// src/integration/cass.rs
use anyhow::Result;
use std::process::Command;

/// Correlate with coding_agent_session_search (cass)
pub fn correlate_session(session_id: &str) -> Result<Option<CassSession>> {
    // Try to find session in cass database
    let output = Command::new("cass")
        .args(["query", "--session-id", session_id, "--format", "json"])
        .output()?;
    
    if !output.status.success() {
        return Ok(None);
    }
    
    let session: CassSession = serde_json::from_slice(&output.stdout)?;
    Ok(Some(session))
}

#[derive(Debug, serde::Deserialize)]
pub struct CassSession {
    pub session_id: String,
    pub agent: String,
    pub started_at: String,
    pub messages: Vec<CassMessage>,
    pub token_usage: CassTokenUsage,
}

#[derive(Debug, serde::Deserialize)]
pub struct CassMessage {
    pub role: String,
    pub content: String,
    pub timestamp: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct CassTokenUsage {
    pub total: u64,
    pub input: u64,
    pub output: u64,
}
```

### 12.2 CodexBar-style Usage Tracking

```rust
// src/integration/usage_tracker.rs
use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Track API usage like CodexBar does
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageInfo {
    pub service: String,
    pub account: String,
    pub limit_type: String,  // "rate" or "usage"
    pub current: f64,
    pub max: f64,
    pub resets_at: Option<String>,
    pub percent_remaining: f64,
}

pub async fn check_openai_usage(api_key: &str) -> Result<UsageInfo> {
    // OpenAI usage API endpoint
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.openai.com/v1/usage")
        .bearer_auth(api_key)
        .send()
        .await?;
    
    // Parse response and calculate usage
    // This is simplified - actual implementation would parse OpenAI's response format
    todo!("Implement OpenAI usage parsing")
}

pub async fn check_anthropic_usage(api_key: &str) -> Result<UsageInfo> {
    // Anthropic usage tracking
    todo!("Implement Anthropic usage parsing")
}

pub fn select_best_account(
    accounts: &[crate::storage::Account],
    usages: &[UsageInfo],
) -> Option<&crate::storage::Account> {
    // Select account with most remaining usage
    accounts.iter()
        .filter_map(|acc| {
            usages.iter()
                .find(|u| u.account == acc.account_name)
                .map(|u| (acc, u.percent_remaining))
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(acc, _)| acc)
}
```

---

## 13. Performance Optimization

### 13.1 Efficient Polling

```rust
// Adaptive polling based on activity
pub struct AdaptivePoller {
    base_interval: Duration,
    min_interval: Duration,
    max_interval: Duration,
    current_interval: Duration,
    last_activity: HashMap<u64, Instant>,
}

impl AdaptivePoller {
    pub fn new() -> Self {
        Self {
            base_interval: Duration::from_millis(500),
            min_interval: Duration::from_millis(100),
            max_interval: Duration::from_secs(5),
            current_interval: Duration::from_millis(500),
            last_activity: HashMap::new(),
        }
    }
    
    pub fn record_activity(&mut self, pane_id: u64) {
        self.last_activity.insert(pane_id, Instant::now());
        // Decrease interval when there's activity
        self.current_interval = self.min_interval;
    }
    
    pub fn tick(&mut self) {
        // Gradually increase interval when idle
        let any_recent = self.last_activity.values()
            .any(|t| t.elapsed() < Duration::from_secs(10));
        
        if !any_recent && self.current_interval < self.max_interval {
            self.current_interval = (self.current_interval * 11 / 10)
                .min(self.max_interval);
        }
    }
    
    pub fn interval(&self) -> Duration {
        self.current_interval
    }
}
```

### 13.2 Incremental Text Capture

```rust
// Only capture changed portions
pub struct IncrementalCapture {
    last_line_count: HashMap<u64, usize>,
    last_hash: HashMap<u64, u64>,
}

impl IncrementalCapture {
    pub fn get_delta(&mut self, pane_id: u64, full_text: &str) -> Option<&str> {
        let current_hash = fxhash::hash64(full_text);
        let current_lines = full_text.lines().count();
        
        if self.last_hash.get(&pane_id) == Some(&current_hash) {
            return None;  // No change
        }
        
        let delta = if let Some(&last_lines) = self.last_line_count.get(&pane_id) {
            if current_lines > last_lines {
                // New lines appended - only return new content
                let skip_bytes: usize = full_text.lines()
                    .take(last_lines)
                    .map(|l| l.len() + 1)
                    .sum();
                &full_text[skip_bytes.min(full_text.len())..]
            } else {
                // Content changed significantly - return all
                full_text
            }
        } else {
            full_text
        };
        
        self.last_line_count.insert(pane_id, current_lines);
        self.last_hash.insert(pane_id, current_hash);
        
        Some(delta)
    }
}
```

### 13.3 Parallel Pane Processing

```rust
use futures::stream::{self, StreamExt};

pub async fn process_all_panes_parallel(
    client: &WeztermClient,
    processor: impl Fn(u64, &str) -> Result<()> + Send + Sync,
) -> Result<()> {
    let panes = client.list_panes().await?;
    
    stream::iter(panes)
        .map(|pane| {
            let client = client.clone();
            async move {
                let text = client.get_text(pane.pane_id, false).await?;
                processor(pane.pane_id, &text)
            }
        })
        .buffer_unordered(10)  // Process up to 10 panes concurrently
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    
    Ok(())
}
```

---

## 14. Safety and Reliability

### 14.1 Graceful Degradation

```rust
// Always have fallback behaviors
pub async fn safe_send_text(
    client: &WeztermClient,
    pane_id: u64,
    text: &str,
) -> Result<()> {
    // Verify pane exists before sending
    let panes = client.list_panes().await?;
    if !panes.iter().any(|p| p.pane_id == pane_id) {
        anyhow::bail!("Pane {} no longer exists", pane_id);
    }
    
    // Check if pane is in alternate screen (vim, etc.)
    // This would require shell integration or heuristics
    
    // Send with retry
    for attempt in 1..=3 {
        match client.send_text(pane_id, text, false).await {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 3 => {
                tracing::warn!("Send attempt {} failed: {}", attempt, e);
                tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
            }
            Err(e) => return Err(e),
        }
    }
    
    unreachable!()
}
```

Backpressure and GAPs (what happens under load):

- Bounded queues: capture and storage use bounded channels. When the capture
  queue is full, the tailer logs a backpressure warning and treats the poll as
  "no change", which naturally backs off the polling interval.
- Storage writer queue: the async storage handle serializes writes through a
  bounded writer queue. A saturated queue increases ingest lag and can delay
  event persistence.
- GAP insertion: if delta extraction cannot guarantee continuity (overlap
  failure, in-place edits, alt-screen toggles, or sequence discontinuity), wa
  records an explicit GAP with a reason and stores the full snapshot to make
  the discontinuity auditable.

Troubleshooting "wa is behind":

- Check `wa status` (or `wa robot state`) for last-seen timestamps and pane
  observation state.
- Inspect logs for backpressure warnings (capture queue full) and GAP reasons
  like `overlap_not_found` or `seq_discontinuity`.
- Use `wa events` / `wa robot events` to confirm whether gap events are being
  emitted for a specific pane.
- If available in your build, `wa doctor` / `wa triage` should summarize ingest
  lag and recent gap reasons.

Tuning knobs (safe mitigations):

- Reduce the number of observed panes with include/exclude filters.
- Increase `ingest.poll_interval_ms` to lower capture pressure.
- Lower `ingest.max_concurrent_captures` to reduce burst load.
- Adjust `storage.writer_queue_size` if the writer queue is persistently full.

### 14.2 State Validation

```rust
// Validate state before taking action
pub async fn validate_before_workflow(
    ctx: &WorkflowContext,
    expected_patterns: &[&str],
) -> Result<bool> {
    let text = ctx.client.get_text(ctx.pane_id, false).await?;
    
    for pattern in expected_patterns {
        if !text.contains(pattern) {
            tracing::warn!(
                "Expected pattern '{}' not found in pane {}",
                pattern,
                ctx.pane_id
            );
            return Ok(false);
        }
    }
    
    Ok(true)
}
```

### 14.3 Audit Logging

```rust
// Log all actions for debugging and compliance
pub fn log_action(
    pane_id: u64,
    action: &str,
    input: Option<&str>,
    result: &str,
) {
    tracing::info!(
        target: "wa::audit",
        pane_id = pane_id,
        action = action,
        input = input,
        result = result,
        "Action executed"
    );
}
```

---

## 15. Complete Implementation Reference

### 15.1 Skills Directory Structure

```
skills/
├── AGENTS.md                    # Main instructions for AI agents using wa
├── workflows/
│   ├── handle_usage_limits.md   # Detailed workflow for account rotation
│   ├── handle_compaction.md     # Context refresh after compaction
│   ├── monitor_progress.md      # Track long-running tasks
│   └── coordinate_agents.md     # Multi-agent coordination patterns
└── patterns/
    ├── claude_code.md           # Claude Code specific patterns
    ├── codex.md                 # Codex CLI specific patterns
    └── gemini.md                # Gemini CLI specific patterns
```

### 15.2 Example AGENTS.md

```markdown
# wa Agent Skills

## Quick Reference

You are controlling AI coding agents through wa. Key capabilities:

1. **Monitor**: Watch for events across all agents
2. **React**: Automatically handle usage limits, compaction, errors
3. **Coordinate**: Manage multiple agents working on related tasks

## Core Commands

```bash
# Get current state of all agents
wa robot state

# Watch for events (run in background)
wa watch --auto-handle &

# Search for specific output
wa robot search "error" --pane-id 3

# Send command to agent
wa robot send 3 "continue with the refactoring"

# Wait for completion signal
wa robot wait-for 3 "Task completed" --timeout-secs 300
```

## Workflow Patterns

### Handling Usage Limits

When an agent hits usage limits:
1. wa detects the pattern
2. Gracefully exits the session (Ctrl-C)
3. Parses session ID
4. Rotates to next available account
5. Resumes session
6. Sends "proceed" prompt

### After Compaction

When context is compacted:
1. wa detects "Conversation compacted" 
2. Waits 1 second for completion
3. Sends: "Reread AGENTS.md so it's still fresh in your mind."

### Coordinating Multiple Agents

Use workspaces to organize:
- `wa robot state` shows all panes
- Filter by title to find specific agents
- Send coordinated instructions
```

### 15.3 Testing Strategy

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_pattern_detection_codex_usage() {
        let detector = PatternDetector::new();
        let text = "⚠ Heads up, you have less than 10% of your 5h limit left.";
        
        let detections = detector.detect(text);
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].agent, AgentType::Codex);
        assert_eq!(detections[0].event, EventType::UsageLimitWarning);
    }
    
    #[test]
    fn test_pattern_detection_claude_code_compaction() {
        let detector = PatternDetector::new();
        let text = "Conversation compacted · ctrl+o for history";
        
        let detections = detector.detect(text);
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].agent, AgentType::ClaudeCode);
        assert_eq!(detections[0].event, EventType::Compaction);
    }
    
    #[test]
    fn test_codex_session_parsing() {
        let text = r#"
Token usage: total=100,117 input=90,506 (+ 3,008,512 cached) output=9,611 (reasoning 7,168)
To continue this session, run codex resume 019bcea5-acb4-7370-a50d-8a2b59553cf6
"#;
        
        let session = crate::patterns::agents::codex::parse_session_end(text).unwrap();
        assert_eq!(session.session_id, "019bcea5-acb4-7370-a50d-8a2b59553cf6");
        assert!(session.token_usage.is_some());
        
        let usage = session.token_usage.unwrap();
        assert_eq!(usage.total, 100117);
        assert_eq!(usage.cached, 3008512);
    }
}
```

---

## Conclusion

This guide provides the complete technical foundation for building `wezterm_automata` (wa), a high-performance Rust CLI for orchestrating AI coding agent fleets. Key takeaways:

1. **WezTerm's native multiplexing** provides the reliable, observable foundation that makes timing-independent automation possible.

2. **The `wezterm cli` commands** are the primary interface - they're stable, well-documented, and support JSON output for easy parsing.

3. **SQLite with FTS5** gives you instant full-text search across all captured terminal output, essential for debugging and analysis.

4. **Aho-Corasick + regex** provides the high-performance pattern matching needed to detect agent events in real-time.

5. **Robot mode** makes wa itself controllable by AI agents, enabling meta-automation where one agent manages a fleet of others.

6. **The workflow engine** codifies common patterns (usage limits, compaction, errors) into reliable, automated responses.

The architecture is designed to be:
- **Deterministic**: Every action is based on observed state, not timing
- **Resilient**: Graceful degradation and retry logic throughout
- **Observable**: Complete audit trail of all actions
- **Extensible**: Easy to add new patterns, workflows, and integrations

Build this tool, and you'll have a robust foundation for scaling AI coding assistance across your entire development infrastructure.
