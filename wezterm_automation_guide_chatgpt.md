Automating WezTerm with Lua for AI Coding Agent Fleet Management

Managing a fleet of coding agents (e.g. Claude, Codex, Gemini CLI, etc.) across multiple remote servers can be streamlined by leveraging WezTerm’s built-in terminal multiplexer and Lua scripting capabilities. This comprehensive guide covers end-to-end automation of WezTerm for such use cases – from setting up persistent remote sessions, to capturing and reacting to terminal output in real-time, to building a high-performance Rust CLI (“wezterm_automaton” or wa) that can manage these agent sessions automatically. We will discuss best practices for maximum performance, reliability, robustness, responsiveness, and safety in this automated environment.

Note: wa removed the Lua `update-status` hook in v0.2.0 due to performance overhead. Any `update-status`
examples below should be treated as historical; prefer CLI polling + user-var signaling + escape-sequence
detection instead.

Why Use WezTerm (and Not tmux)?

WezTerm is a GPU-accelerated terminal emulator that also functions as a multiplexer, which means it can replace tmux in many scenarios ￼ ￼. Unlike tmux, WezTerm integrates directly with the terminal UI, eliminating the need for nested sessions and providing a more seamless experience. The table below summarizes key differences:

Feature	Using tmux (nested)	Using WezTerm Mux (native)
Scrollback	Nested (scrollback within scrollback – can be confusing)	Native terminal scrollback (one layer) ￼
Keybindings	Requires prefix (e.g. Ctrl+B) – can conflict with terminal shortcuts	Single namespace (WezTerm handles multiplexing commands directly)
Rendering	Text-only, CPU-rendered	Full GPU acceleration for text rendering ￼
Mouse support	Limited (tmux needs config for mouse)	Native mouse support (selection, clicks)
Setup overhead	tmux installed & configured on each remote; separate config	WezTerm on each machine; uses one unified Lua config
UI and Theming	Basic text UI (limited styling)	Rich UI: full colors, gradients, styled tab bar, images, emoji
Integration	Adds extra IO layer in terminal ￼ (potential friction)	Integrated multiplexer (no extra layer; direct integration with terminal)
Session Persistence	Requires care (tmux server must keep running; often solved via tmux attach)	Built-in persistence via wezterm-mux-server (with systemd, sessions survive disconnection)

If you are happy with tmux’s workflow, you can continue using it. But WezTerm’s native multiplexer offers a more modern, smoothly integrated approach – especially useful when managing multiple remote AI agent sessions simultaneously. WezTerm’s Lua scripting allows dynamic configuration and automation beyond tmux’s static config ￼ ￼, which we will leverage heavily.

Prerequisites

Before diving into automation, ensure you have the following in place:
	•	WezTerm (latest version) on your local machine (e.g. Mac). The same version must be installed on all remote machines to ensure protocol compatibility ￼. For example, if using release 20240101 on Mac, install 20240101 on each server.
	•	WezTerm on remote servers installed and accessible in $PATH. WezTerm’s SSH multiplexer requires a compatible wezterm binary remotely ￼.
	•	SSH access from the local machine to each server. Key-based authentication is recommended for automation (to avoid password prompts).
	•	systemd user session available on each server (common on modern Linux) to run the WezTerm multiplexer as a background service.
	•	Basic knowledge of Lua for configuring WezTerm, and Rust (if implementing the CLI tool).

Ensure Matching WezTerm Versions

Check versions on local and remote:

# On local (Mac):
wezterm --version

# On each remote:
ssh user@remote "wezterm --version"

They should match exactly (e.g. WezTerm 20240101-123456-abcdef). If not, update accordingly so that the WezTerm CLI and multiplexer protocol align.

WezTerm Persistent Remote Sessions Overview

WezTerm can run a persistent mux server on each remote host, so that your terminal sessions remain alive on the server even if your local GUI disconnects (similar to tmux, but using WezTerm’s native capabilities). The local WezTerm GUI can then reconnect to those sessions at any time. When configured properly, this means if your laptop sleeps or reboots, you can reattach to ongoing agent sessions without losing their state.

How it works: WezTerm’s SSH domain feature will, under the hood, SSH to the server, launch (or connect to) wezterm-mux-server on that host, and then proxy the terminal session. We’ll use systemd to keep wezterm-mux-server running persistently on each server. The diagram below illustrates the architecture:

# Local WezTerm GUI (macOS) with multiple windows/tabs connected to remote mux servers

┌─────────────────────────────────────────────────────────────────────────────┐
│                           YOUR MAC (WezTerm GUI)                             │
│                                                                              │
│   ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐     │
│   │ 󰍹  Local    │  │ 󰒋  Dev      │  │ 󰒋  Staging  │  │ 󰻠  Workstation│     │
│   │ (local tabs) │  │ (SSH→purple)│  │ (SSH→amber)  │  │ (SSH→crimson) │     │
│   │  [3 tabs]    │  │  [3 tabs]   │  │  [3 tabs]   │  │  [3 tabs]     │     │
│   └──────────────┘  └──────┬───────┘  └──────┬───────┘  └───────┬───────┘     │
│          │                 │                 │                  │            │
│     (fresh each       SSH+Mux           SSH+Mux            SSH+Mux           │
│      startup)       (persistent)      (persistent)       (persistent)        │
│           │                 │                 │                  │            │
└───────────┼─────────────────┼─────────────────┼──────────────────┼────────────┘
            ▼                 ▼                 ▼                  ▼
    ┌──────────────────────────────────────────────────────────────────────┐
    │                    REMOTE SERVERS (WezTerm mux)                      │
    │                                                                      │
    │  dev-server          staging           workstation                   │
    │  10.20.30.1          10.20.30.2        192.168.1.50                   │
    │                                                                      │
    │  wezterm-mux-server  wezterm-mux-server  wezterm-mux-server           │
    │  (systemd service)   (systemd service)   (systemd service)           │
    │                                                                      │
    │  Sessions persist here – survive Mac sleep/reboot, network drops,    │
    │  even laptop power loss. Reattach from Mac seamlessly via SSH.       │
    └──────────────────────────────────────────────────────────────────────┘

In this setup, the local WezTerm opens four windows: one local (for local shells), and three that connect to remote domains (“Dev”, “Staging”, “Workstation”). The remote windows attach to existing sessions on the server if present, or start new ones if none exist. Each remote window shown has multiple tabs (which correspond to multiple shells running on that remote, within the single mux server process).

Setting Up Persistent Remote WezTerm Sessions

To enable the above, perform these steps on each remote server:

1. Install WezTerm on the Remote

Install the same version of WezTerm on the remote. For example, on Ubuntu/Debian:

curl -fsSL https://apt.fury.io/wez/gpg.key | sudo gpg --dearmor -o /usr/share/keyrings/wezterm-fury.gpg
echo "deb [signed-by=/usr/share/keyrings/wezterm-fury.gpg] https://apt.fury.io/wez/ * *" | sudo tee /etc/apt/sources.list.d/wezterm.list
sudo apt update && sudo apt install wezterm
wezterm --version  # confirm version

On other distros or macOS servers, install via appropriate package or Homebrew, matching the version.

2. Create a systemd User Service for wezterm-mux-server

WezTerm provides a wezterm-mux-server command that runs a headless mux daemon. We will run this as a user service so it stays alive in the background.

On the remote server, create a user service file:

mkdir -p ~/.config/systemd/user
nano ~/.config/systemd/user/wezterm-mux-server.service

Put the following into wezterm-mux-server.service:

[Unit]
Description=WezTerm Multiplexer Server
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/wezterm-mux-server --daemonize=false
Restart=on-failure
RestartSec=5
# Optional: reduce logging verbosity
Environment=WEZTERM_LOG=warn

[Install]
WantedBy=default.target

Explanation: This service starts wezterm-mux-server for your user at login, keeps it running, and restarts on failure. We set --daemonize=false because systemd will manage the process (letting it daemonize itself isn’t needed).

3. Enable Linger (Keep Session Alive After Logout)

By default, systemd user services stop when you log out (no active session). To allow wezterm-mux-server to run even with no SSH sessions, enable linger for your user:

# Enable user lingering to keep user services running after logout
sudo loginctl enable-linger $USER

Enabling linger ensures your user’s systemd instance continues running after all interactive sessions close ￼. This is critical so that the mux server isn’t killed when you disconnect SSH or your local WezTerm closes.

Note: On some systems, you might need to log out and back in (or systemctl --user start wezterm-mux-server) for the linger setting to take effect immediately ￼ ￼.

4. Start the WezTerm Mux Service

Reload systemd user units and start the service now:

systemctl --user daemon-reload
systemctl --user enable --now wezterm-mux-server

This should launch wezterm-mux-server in the background. Verify its status:

systemctl --user status wezterm-mux-server

You should see it active (running). If it failed, check logs with journalctl --user -u wezterm-mux-server -n 100 for errors.

5. (Optional) Remote Shell Config for Login Shells

WezTerm does not automatically invoke login shells on multiplexer sessions. If you want your remote tabs to start as login shells (to load /etc/profile, etc.), you can create a minimal WezTerm config on the remote:

nano ~/.wezterm.lua

local wezterm = require 'wezterm'
return {
  default_prog = { '/bin/bash', '-l' }  -- or your shell
}

This ensures that when the mux spawns shells, they are login shells (especially useful if using bash/zsh that need -l flag to load profiles).

Repeat steps 1–5 on each remote server you plan to use.

Local WezTerm Configuration for Automation

All the magic happens in the local WezTerm’s config (~/.wezterm.lua), where we define SSH domains, UI customization, and automation hooks in Lua. Below, we outline a configuration that achieves the following:
	•	Defines SSH domains for each remote, with WezTerm multiplexing enabled.
	•	Assigns distinct colors and appearance for each domain (so you can visually distinguish dev vs staging vs workstation).
	•	On startup, uses a smart startup routine to open multiple windows/tabs (local and remote) and avoid duplicate tabs if reconnecting to existing sessions.
	•	Sets up custom keybindings for efficient navigation and control (including a “Leader” key like tmux).
	•	Prepares hooks for capturing output and reacting to events (we will expand on this for automation).

<details>
<summary>🔧 <strong>Example ~/.wezterm.lua</strong> (Click to expand)</summary>


local wezterm = require 'wezterm'
local config = wezterm.config_builder()

-- SSH DOMAINS: Define remote servers for WezTerm mux connections
config.ssh_domains = {
  {
    name = 'dev-server',       -- identifier used in domain
    remote_address = '10.20.30.1',
    username = 'ubuntu',
    multiplexing = 'WezTerm',  -- use WezTerm mux (not raw SSH)
    assume_shell = 'Posix',    -- remote is a Unix-like shell
  },
  {
    name = 'staging',
    remote_address = '10.20.30.2',
    username = 'ubuntu',
    multiplexing = 'WezTerm',
    assume_shell = 'Posix',
  },
  {
    name = 'workstation',
    remote_address = '192.168.1.50',
    username = 'dev',
    multiplexing = 'WezTerm',
    assume_shell = 'Posix',
  },
}

-- DOMAIN-SPECIFIC COLORS: to differentiate remote sessions visually
local domain_colors = {
  ['dev-server'] = {  -- purple theme for Dev
    background = { { source = { Gradient = {
                    colors = { '#1a0d1a', '#2e1a2e', '#3e163e' },
                    orientation = { Linear = { angle = -45.0 } },
                  }} , width='100%', height='100%', opacity=0.9 } },
    colors = {
      foreground = '#cfcffa', background = '#1a0d1a',
      cursor_bg = '#bb9af7', cursor_border = '#bb9af7', split = '#bb9af7',
      tab_bar = { background = 'rgba(26,13,26,0.9)',
        active_tab = { bg_color='#bb9af7', fg_color='#1a0d1a', intensity='Bold' },
        inactive_tab = { bg_color='#2e1a2e', fg_color='#9070a0' },
        inactive_tab_hover = { bg_color='#3e163e', fg_color='#bb9af7' },
      },
    },
  },
  ['staging'] = {  -- amber theme for Staging
    background = { { source = { Gradient = {
                    colors = { '#1a0f05', '#2e1a10', '#3e2116' },
                    orientation = { Linear = { angle = -45.0 } },
                  }} , width='100%', height='100%', opacity=0.9 } },
    colors = {
      foreground = '#fff1dc', background = '#1a0f05',
      cursor_bg = '#e0af68', cursor_border = '#e0af68', split = '#e0af68',
      tab_bar = { background = 'rgba(26,15,5,0.9)',
        active_tab = { bg_color='#e0af68', fg_color='#1a0f05', intensity='Bold' },
        inactive_tab = { bg_color='#2e1a10', fg_color='#a08060' },
        inactive_tab_hover = { bg_color='#3e2116', fg_color='#e0af68' },
      },
    },
  },
  ['workstation'] = {  -- crimson theme for Workstation
    background = { { source = { Gradient = {
                    colors = { '#1a0a0a', '#2e1416', '#3e1a1c' },
                    orientation = { Linear = { angle = -45.0 } },
                  }} , width='100%', height='100%', opacity=0.9 } },
    colors = {
      foreground = '#ffdce0', background = '#1a0a0a',
      cursor_bg = '#dc143c', cursor_border = '#dc143c', split = '#dc143c',
      tab_bar = { background = 'rgba(26,10,10,0.9)',
        active_tab = { bg_color='#dc143c', fg_color='#ffffff', intensity='Bold' },
        inactive_tab = { bg_color='#2e1416', fg_color='#a06070' },
        inactive_tab_hover = { bg_color='#3e1a1c', fg_color='#dc143c' },
      },
    },
  },
}

-- Optional: human-friendly names and icons for domains (for status display)
local domain_info = {
  ['dev-server']  = { name = 'Dev Server',   icon = '󰒋 ', color = '#bb9af7' },
  ['staging']     = { name = 'Staging',      icon = '󰒋 ', color = '#e0af68' },
  ['workstation'] = { name = 'Workstation',  icon = '󰻠 ', color = '#dc143c' },
}

-- DYNAMIC OVERRIDES ON DOMAIN CHANGE: apply colors and status when active pane's domain changes
-- NOTE: update-status was removed in wa v0.2.0. This example is HISTORICAL.
-- Prefer CLI polling + user-var signaling instead.
local last_domain = {}
wezterm.on('update-status', function(window, pane)  -- DEPRECATED: see note above
  local domain = pane:get_domain_name()
  local winid = tostring(window:window_id())
  if last_domain[winid] ~= domain then
    last_domain[winid] = domain
    local overrides = window:get_config_overrides() or {}
    if domain_colors[domain] then
      overrides.colors = domain_colors[domain].colors
      overrides.background = domain_colors[domain].background
    else
      overrides.colors = nil
      overrides.background = nil
    end
    window:set_config_overrides(overrides)
  end
  -- Right-side status: show domain name badge
  local info = domain_info[domain]
  if info then
    window:set_right_status(wezterm.format({
      {Foreground={Color='#0d0d1a'}}, {Background={Color=info.color}},
      {Attribute={Intensity='Bold'}}, 
      {Text=' ' .. info.icon .. info.name .. ' ' },
    }))
  else
    window:set_right_status('')
  end
end)

-- SMART STARTUP (gui-startup event): open windows/tabs and avoid duplicates
local remote_domains = {
  { name = 'dev-server',  cwd = '/data/projects' },
  { name = 'staging',     cwd = '/var/www' },
  { name = 'workstation', cwd = '/home/dev/code' },
}
local TABS_PER_WINDOW = 3

wezterm.on('gui-startup', function(cmd)
  -- 1. Create local window with multiple tabs
  local project_dir = wezterm.home_dir .. '/projects'
  local _, _, local_window = wezterm.mux.spawn_window{ cwd = project_dir }
  for i = 2, TABS_PER_WINDOW do
    local_window:spawn_tab{ cwd = project_dir }
  end
  -- 2. Create or attach to remote windows for each domain
  for _, remote in ipairs(remote_domains) do
    local domain_name = remote.name
    local ok, err = pcall(function()
      local _, _, window = wezterm.mux.spawn_window{
        domain = { DomainName = domain_name },
        cwd = remote.cwd or '~'
      }
      -- If the remote mux has no tabs yet (fresh session), create tabs
      local tabs = window:tabs()
      if #tabs <= 1 then
        for i = 2, TABS_PER_WINDOW do
          window:spawn_tab{ cwd = remote.cwd or '~' }
        end
      end
      -- If there were already tabs, we don't duplicate them.
    end)
    if not ok then
      wezterm.log_warn('Could not connect to ' .. domain_name .. ': ' .. err)
    end
  end
end)

-- KEYBINDINGS & LEADER KEY
config.leader = { key='a', mods='CTRL', timeout_milliseconds=1000 }  -- Ctrl+A as leader
config.keys = {
  -- Quick new tab in specific domain (Leader+1/2/3)
  { key='1', mods='LEADER', action=wezterm.action.SpawnCommandInNewTab{ domain={DomainName='dev-server'}, cwd='/data/projects' } },
  { key='2', mods='LEADER', action=wezterm.action.SpawnCommandInNewTab{ domain={DomainName='staging'}, cwd='/var/www' } },
  { key='3', mods='LEADER', action=wezterm.action.SpawnCommandInNewTab{ domain={DomainName='workstation'}, cwd='/home/dev/code' } },
  -- Tab navigation shortcuts (Ctrl+Shift+←/→ to switch tabs)
  { key='LeftArrow',  mods='CTRL|SHIFT', action=wezterm.action.ActivateTabRelative(-1) },
  { key='RightArrow', mods='CTRL|SHIFT', action=wezterm.action.ActivateTabRelative(1) },
  -- Leader+w opens domain launcher UI (choose domain to spawn tab)
  { key='w', mods='LEADER', action=wezterm.action.ShowLauncherArgs{ flags='DOMAINS' } },
}

return config

</details>


Let’s break down the important parts of this configuration:
	•	SSH Domains: We define named domains for each remote. Setting multiplexing = 'WezTerm' tells WezTerm to use its own mux protocol (not just a raw SSH). With this, wezterm connect <name> or domain-specific spawns will auto-start the remote mux server if needed ￼. (Ensure the remote wezterm-mux-server is running as we set up in systemd.)
	•	Domain Colors: A Lua table domain_colors maps domain names to a color scheme (background gradient and color settings). In the update-status event, whenever the active pane’s domain changes, we call window:set_config_overrides to dynamically apply these colors. This way, each remote gets a distinct look (purple, amber, crimson in our example), reducing the chance of typing in the wrong server.
	•	Domain Badge in Status: Using window:set_right_status inside update-status to show a label like “[Dev Server]” with an icon on the right side of the tab bar. This uses WezTerm’s formatting markup to style text. It’s purely cosmetic but very helpful to keep track of where you are.
	•	Smart Startup (gui-startup): When the GUI launches, we programmatically create windows and tabs:
	•	One local window with 3 tabs (for local work).
	•	For each remote domain, we attempt to connect (spawn a window). If the remote mux is fresh (no tabs yet), we create 3 tabs on it. If it already had tabs (from a previous run), we do nothing, so as not to duplicate. This logic ensures that when you restart WezTerm, you get your local window anew, but your remote windows restore to exactly how you left them (thanks to the persistent mux on the server) ￼ ￼. It prevents “tab explosion” on subsequent launches.
	•	We wrap each remote spawn in pcall to catch errors (e.g., if a server is down or not reachable, it won’t crash the whole config).
	•	Keybindings and Leader: We set a tmux-like Leader (Ctrl+A) and bind Leader+1/2/3 to quickly open a new tab in a specific remote domain (e.g., press Ctrl+A then 2 to open a new tab on Staging server, in /var/www). We also bind Ctrl+Shift+←/→ for tab navigation (like browser tabs), and Leader+w to open a domain launcher (WezTerm’s builtin UI to select a domain to connect to).

Tip: The above config uses WezTerm’s latest features like wezterm.mux APIs for spawning windows/tabs and dynamic color overrides. Be sure you’re on a recent version (2024+).

After writing your config, launch WezTerm. If all is well, you should see multiple windows appear: one local and one per remote. The remote ones might take a few seconds to connect if authentication is happening. Once connected, each remote window should show 3 tabs (either new shells or your pre-existing ones). You can now seamlessly work as if all those sessions were local tabs, with WezTerm handling the networking and persistence.

Usage and Maintenance of Remote Mux Sessions

Once set up, here are some useful commands and practices for daily operation:
	•	Monitoring mux status: You can manually check that the remote mux server is running:

ssh dev-server "systemctl --user status wezterm-mux-server"

This should show it active. Likewise, check other servers. (In practice, if it’s not running, WezTerm will try to start a temporary one on connect, but that defeats persistence.)

	•	Restarting a remote mux server: If you need to reset the remote session state (for example, if something went wrong or you want to clear all tabs on that remote), you can restart the service:

ssh dev-server "systemctl --user restart wezterm-mux-server"

This will kill all remote persistent sessions (like doing a full tmux reset). After that, if you reconnect (launch WezTerm or spawn a tab to that domain), you’ll get a fresh session. Use with caution in production environments.

	•	Viewing WezTerm mux logs: The mux server logs to the user journal. For debugging:

ssh dev-server "journalctl --user -u wezterm-mux-server --since '1 hour ago'"

Adjust time as needed. This can help if sessions aren’t persisting or the service is flapping.

	•	Rescuing a stuck session: If a remote tab becomes unresponsive or misbehaves, you can try killing that pane without affecting others:
	1.	Identify its pane ID via wezterm cli list (see next section for using this command).
	2.	Run wezterm cli kill-pane --pane-id <PANEID> from your local machine. This is akin to closing that tab/session.
	•	Upgrading WezTerm: When a new version is released, plan to update both local and remote. Mismatched versions can cause connection failures or weird behavior (WezTerm will usually warn if versions differ). It’s best to stop the remote mux service, upgrade both sides, then start it again.

WezTerm CLI for Introspection and Control

WezTerm provides a wezterm cli tool which can interact with the running WezTerm instance (the local GUI or a mux). This is extremely useful for automation. Some key subcommands:
	•	wezterm cli list – Lists all open windows, tabs, and panes currently managed by WezTerm. This works for both local and connected remote sessions. By default it prints a table; use --format json for machine-readable output ￼. For example:

wezterm cli list --format json

might output:

[
  {
    "window_id": 0, "tab_id": 0, "pane_id": 0, "workspace": "default",
    "size": {"cols":80,"rows":24},
    "title": "user@dev-server:~", 
    "cwd": "file://dev-server/home/ubuntu"
  },
  { "window_id": 0, "tab_id": 1, "pane_id": 1, ... },
  { "window_id": 1, "tab_id": 0, "pane_id": 2, ... },
  ...
]

Each entry is a pane. Notably it shows pane_id (unique across all windows), the title (which often includes the remote user@host and current directory or running command), and cwd (as a URI, with remote host). Our automation tool will use this to discover what sessions exist and identify them (e.g. which pane corresponds to which agent).

	•	wezterm cli send-text – Sends text input to a pane as though it was typed/pasted ￼ ￼. You can target a specific pane with --pane-id. By default it simulates a pasted block of text (bracketed paste), which won’t implicitly press Enter. You can use --no-paste to send raw keystrokes. For example:

wezterm cli send-text --pane-id 3 --no-paste "ls -la\r"

will send ls -la and a carriage return (the \r simulates pressing Enter) to pane 3. This is how we can programmatically execute commands in any session. Note: When using --no-paste, you can include control characters like \r (newline) and even escape sequences. This is how to “press” Enter; otherwise, without --no-paste, the text would be bracket-pasted (which the shell would not execute until you manually hit Enter) ￼ ￼. In PowerShell or some shells, you might need a different newline sequence, but for bash/zsh \r is fine.

	•	wezterm cli get-text – Grabs the text content of a pane’s visible screen (and optionally scrollback) and prints it to stdout ￼ ￼. You can use --pane-id and also --start-line/--end-line to specify the range. This is extremely powerful for automation: it lets us read what’s happening in the terminal. For example:

wezterm cli get-text --pane-id 3 --start-line -20

would output the last 20 lines of scrollback (the negative index means lines from the bottom of the buffer) ￼. If omitted, it captures just the main screen (not scrollback). We can use this to monitor agent outputs.

	•	Other useful commands: wezterm cli spawn to launch a new process/tab in a running WezTerm, wezterm cli split-pane to split an existing pane, wezterm cli activate-tab or activate-pane to switch focus (though for automation, focus is less important). Also wezterm cli set-tab-title can rename tabs (perhaps to label them with agent names).

These CLI commands allow external scripts/tools to control and observe the state of WezTerm. They are the backbone of how our wezterm_automaton (wa) Rust tool will interface with WezTerm.

Automating Agent Sessions: Observing and Reacting to Terminal Output

With the infrastructure in place, the next step is automating the behavior of your AI coding agent sessions. This means:
	1.	Capturing all output from all sessions (tabs) in real time.
	2.	Detecting specific patterns in the output that indicate certain events (e.g. conversation compaction, usage limits, errors).
	3.	Taking actions (sending input commands, logging info, switching accounts, etc.) in response to those detections.
	4.	Logging everything to a durable store (like SQLite) for analysis, history, or search.

Capturing Terminal Output Streams

As noted, WezTerm (like any terminal) doesn’t inherently know the semantic meaning of output, but we can retrieve the text being displayed. There are two ways to capture output:
	•	Pull-based (external): Have an external process (our Rust CLI) periodically poll each pane for new text via wezterm cli get-text.
	•	Push-based (in-config Lua): Use WezTerm’s Lua event system to detect output and send it somewhere (not straightforward, since WezTerm doesn’t emit an event for every line of output; we might simulate by using timers or the status updates).

Recommended: Use the external polling approach for simplicity and reliability. Modern WezTerm versions are efficient enough to handle frequent get-text calls, and it keeps the complex logic out of the GUI process (so a bug in your Lua doesn’t freeze your terminal). We will proceed with that design.

Strategy for polling: We want to avoid re-reading the entire scrollback repeatedly (could be thousands of lines, which is inefficient). Instead, we track how much we’ve already read and only fetch new output incrementally.

One approach is:
	•	Initially, capture the full scrollback of each pane and store it (in memory or DB).
	•	Note the number of lines or a marker for the end.
	•	Periodically (say every second, or adaptive intervals), use wezterm cli get-text --start-line <last_read_line> to fetch anything new since last read.

WezTerm’s get-text supports negative indices for start/end relative to bottom ￼. Alternatively, within the config, the Lua Pane API offers functions like pane:get_lines_as_text(N) which returns the last N lines of the pane’s scrollback+screen ￼. In fact, WezTerm’s docs show an example of writing the entire scrollback to a file and opening it in an editor using this method ￼ ￼. We could use a similar approach to grab new lines.

A pseudo-code for external polling:

for each pane:
    new_text = wezterm cli get-text --pane-id X --start-line <last_known_line_index>
    if new_text not empty:
        append to log (with timestamp, pane id, etc.)
        update last_known_line_index

However, determining last_known_line_index in absolute terms is tricky because the scrollback is a circular buffer. Instead, a simpler method:
	•	Always get the last N lines (for some N slightly larger than the expected new output per interval, e.g. 100 lines).
	•	Keep an internal buffer to compare overlap with what was previously seen to filter out duplicates.

Alternatively, use has_unseen_output (WezTerm sets a flag when output arrives to an unfocused pane ￼). But that only tells if something changed, not what.

For robust logging, the safest is to just retrieve recent output slices and parse. Since we’re building a high-performance Rust tool, we can afford some string handling. The Rust regex crate is very fast (it even uses SIMD for some searches) and can handle multi-line patterns efficiently ￼.

Storing logs: We plan to use SQLite to store all output from all sessions. A suggested schema might be:

CREATE TABLE terminal_log (
    id INTEGER PRIMARY KEY,
    timestamp TEXT,          -- or numeric
    domain TEXT,             -- e.g. 'dev-server'
    pane_id INTEGER,         -- WezTerm pane id
    line TEXT                -- the text content of the line
);

We could store one row per line of output with a timestamp. This allows easy querying (e.g. search all logs for “error” or for specific patterns). SQLite can handle many thousands of inserts per second if batched, but since our output is mostly text from coding sessions, it’s manageable. We might insert in chunks for efficiency.

Tip: To avoid duplicating lines if we poll frequently, we can use the combination of [pane_id, line_content, last_seen_timestamp] in memory to decide if a line is already logged. Or simply rely on the fact that if we always append only new lines beyond last index, duplication won’t occur.

WezTerm’s internal representation distinguishes the prompt vs output lines simply by order, so we capture exactly what a user would see. If color codes or ASCII art are present, get-text by default strips color (we can log plain text, which is easier for search). If needed, --escapes could preserve ANSI color codes, but that’s usually not necessary for analysis.

Detecting Important Events via Patterns

With all output being aggregated, we can now watch for specific trigger phrases or patterns that indicate events where automation is needed. Here are examples of such patterns from different AI coding agents:
	•	Conversation compaction (Claude Code): When Claude’s context is full and it compacts the conversation, it prints a distinctive banner and list of what it retained. For example:

Conversation compacted · ctrl+o for history
════════════════════════════════════════════════════════════════════════
  ⎿  Read x/registry/keeper/signature_test.go (58 lines)
  ⎿  Read tools/harness/orchestrator/runner.go (285 lines)
  ⎿  Todo list read (5 items)

The key trigger is the phrase “compacted” coupled with the special line of ══. We can detect "Conversation compacted" in the output. When this appears, it means the agent has just forgotten some older context and only the listed items remain in its memory.
Automation response: It’s often beneficial to re-prime the agent after compaction. For instance, you might want to remind Claude of certain documents or constraints again. In the user’s workflow, they send a prompt like “Reread AGENTS.md so it’s fresh in your mind.” after compaction. We can automate this: once we detect the compaction line, our tool can automatically send-text that prompt to the same pane ￼. This ensures the agent immediately reloads critical context it might have dropped.

	•	Usage limit warnings (Codex): The OpenAI Codex CLI provides warnings as you approach usage limits:
	•	“⚠ Heads up, you have less than 25% of your 5h limit left.”
	•	“⚠ Heads up, you have less than 5% of your 5h limit left.”
	•	Finally, when exhausted: “You’ve hit your usage limit. Visit … to purchase more or try again at [time].”
These lines start with a warning sign (⚠) and mention percentages or the phrase “hit your usage limit”. They’re easily matchable. The final message indicates the agent will not continue.
Automation response: Upon “hit your usage limit” detection, the strategy can be:
	1.	Gracefully terminate the session. Often pressing Ctrl+C twice will exit the Codex CLI. Sending \x03 (ETX, Ctrl+C) via send-text --no-paste can simulate that (though WezTerm doesn’t have a direct “send Ctrl+C” command, the control code can be sent in a no-paste context).
	2.	When Codex exits, it typically prints a summary like:

Token usage: total=100,117 input=90,506 (+3,008,512 cached) output=9,611
To continue this session, run codex resume 019bcea5-acb4-7370-a50d-8a2b59553cf6

Our logger should capture this. We can parse out the resume session ID UUID.

	3.	Trigger an automatic account switch: The user might have multiple OpenAI accounts or API keys to bypass the limit. Codex CLI supports logging in via device auth. So our tool could:
	•	Run cod logout (if needed) then cod login --device-auth in that pane.
	•	This will output a URL and a device code (e.g. https://auth.openai.com/codex/device and a code like 1F4J-LS4XN).
	•	Our tool can automate the browser step: using a headless browser (Playwright or similar), navigate to the URL, log in with the next account’s credentials (stored securely beforehand), and enter the code. This completes the auth without user intervention.
	•	The CLI will then output “Successfully logged in”.
	4.	Finally, use the captured session ID to run cod resume <ID> to continue the session with the new account. Then perhaps send a message like "Proceed." to let the agent continue where it left off.
All of these sub-steps can be automated and scripted in the tool. The key is that the trigger (“hit your usage limit”) was caught reliably, and we were able to parse the follow-up info (resume ID, device code) from the terminal text.

	•	Usage limit reached (Claude or Gemini): Similar situations occur with Anthropic’s Claude (in Claude Code CLI) and Google’s Gemini (via their CLI):
	•	Claude Code might say: “You’ve hit your limit · resets 5pm (America/Chicago) /upgrade to increase your usage limit.” At that point, no further prompts are accepted.
	•	Gemini’s CLI might show a message in a bordered box: “Usage limit reached for all Pro models. /stats for usage details /model to switch models.” etc.
Automation response: For Claude, you might have multiple accounts or an API key rotation – automate a logout/login or key swap similar to Codex’s case. For Gemini, maybe switch to a different model or use an API key if available (as suggested by the CLI output).
	•	Session end or errors: If an agent process crashes or ends unexpectedly, WezTerm’s pane will close. We could detect that by wezterm cli list (pane disappears) or perhaps a “Connection failed” message in WezTerm. In such cases, automation might attempt to restart the agent. For example, if the codex process died, we can spawn a new one in that tab:

wezterm cli spawn --pane-id <pane_that_closed> -- cwd ~ -- bash -c "cod resume <last_session_id>"

(If the pane is truly gone, we might instead reopen a new tab in the same domain.)

	•	Long-running command completion: If needed, WezTerm’s shell integration can emit notifications when a shell command finishes (OSC 133; similar to iTerm2). If configured, our tool could detect when a heavy compile or test run is done (shell could set a user var or bell). WezTerm does have an event for bell (bell event in Lua). If agents trigger a bell (some CLI might do that on certain events), the config could catch it ￼, and we could use that to highlight something or log it.

In summary, we will set up a pattern matching engine in our Rust tool that continuously scans incoming text for these triggers. Given the volume of text can be large, we use efficient searching:
	•	Use Rust’s regex crate (which compiles regexes to efficient automata, uses multi-threading and SIMD where possible) ￼ ￼.
	•	Precompile all relevant patterns (compaction, usage warnings, etc.) into regex objects at program start.
	•	Alternatively, for simple fixed phrases, use the Aho-Corasick algorithm (the Rust aho-corasick crate, used internally by regex, is great for multiple string patterns search) ￼.
	•	We can even limit searches to just the new text chunk that arrived, rather than rescanning everything.

Example: If new_text = “⚠ Heads up, you have less than 5%…” we run it through our regex patterns:
	•	Pattern for /hit your usage limit/ won’t match, but pattern for /less than (\d+)% of your .* limit/ will match and we can extract “5%” if needed.
	•	We might simply log warnings but only auto-react to the final “hit your limit”.
	•	Our code would look for something like r"hit your limit" or the exact phrasing.

For compaction:
	•	Regex like r/Conversation compacted/ suffices (no need for full fidelity on the fancy unicode box drawing, we just need to know compaction happened).
	•	Once matched, we enqueue sending the reminder prompt.

Because the automation operates asynchronously relative to the terminal output, be mindful of timing:
	•	We might detect “hit limit” and send Ctrl+C immediately. It could happen before the agent prints the resume ID. A better approach is to wait a short moment or to detect the prompt disappearance.
	•	Possibly, monitor the process state: e.g., after usage hit, the CLI might not accept input until you restart anyway. But if it does print a resume ID only after you Ctrl+C, we might need to wait for that to appear in logs after sending the interrupt.

A robust solution is to implement a small state machine per agent session:
	•	States like: Running, LimitWarning, LimitReached, ReauthInProgress, AwaitingResume, etc.
	•	Transitions triggered by regex matches or by actions taken.

For instance, upon “hit limit”, enter LimitReached state:
	•	Action: send Ctrl+C to get session summary (if the CLI doesn’t auto-exit).
	•	Then wait for “resume ” line. When that appears, capture ID, move to HaveResumeID state.
	•	Then trigger re-auth. When login succeeds (detect “Successfully logged in”), go to ReauthDone state.
	•	Then send “cod resume ID” and then “proceed.” and back to Running state.

All of this can be done without user intervention.

Implementing the wezterm_automaton (wa) CLI Tool

Now we bring it together by building a dedicated Rust CLI tool, which we’ll call wa (short for WezTerm Automaton). The design goals for this tool:
	•	Agent-first interface: It should be easily usable by AI coding agents themselves (not just humans). That means providing outputs in structured formats (JSON or concise Markdown) that are easy to parse. It also means having a clear “robot mode” with minimal non-determinism.
	•	High performance: It should handle rapid output from multiple sessions without lag, thanks to Rust’s efficiency and concurrency.
	•	Cross-platform: Rust and WezTerm are cross-platform, so wa should run wherever WezTerm runs (Linux, macOS, Windows WSL).
	•	Ease of use for humans: It can also double as a power-user tool for humans, e.g. to quickly search logs or send canned commands to all agents.

Key Functionalities
	1.	Session Discovery: When wa starts (or on demand), it will use wezterm cli list --format json to enumerate all current sessions ￼. This provides pane IDs, titles, CWDs, etc. wa can present these in a quick summary (e.g., list of domains and active processes) or use them internally to map which pane corresponds to which agent.
If your agents have identifiable prompts or titles, wa could auto-label them. For example, Codex’s title might show as node /path/to/codex.js -- user@host:dir or similar; Claude might just be a bash process. We may instead identify agent sessions by a heuristic:
	•	If the pane title contains the agent name or a known token (like “Claude” or “codex”), or
	•	By looking at the first few lines of output (which usually include a welcome message unique to each agent, e.g. “Claude Code vX.Y.Z” or “Welcome to Codex…”).
	•	We could also require the user/agent to tag a tab by name (WezTerm allows renaming tab title manually or via script) and look for that. But let’s assume we can identify by content.
	2.	Continuous Monitoring: wa will spawn background threads or an async task per pane to poll for new output every second (or even more frequently if needed, though 1Hz is often enough for interactive usage). It will compare and log new lines, and run pattern matching on them.
	3.	Reactive Actions: When a pattern is matched, wa triggers the corresponding handler. This might involve printing a message to its own output (for human/agent info) and performing a wezterm cli send-text to the terminal. For potentially complex sequences (like re-auth flow), it might spawn a sub-task that orchestrates multiple steps with some delays as needed:
	•	e.g., after sending login and launching Playwright to do the web login, wa will need to wait for that to complete (with a timeout) and then send resume command.
	4.	Logging to SQLite: wa will open (or create) a SQLite database file (perhaps ~/.wezterm_automaton/log.sqlite). As new lines come in, it will insert them. It could batch inserts for efficiency. We might also provide a way to query this DB (like a subcommand wa search "error" that does an SQL query or uses FTS if configured).
	5.	Expose a Command Interface: The tool should allow the agent (or user) to query the state and perform operations. For example:
	•	wa status – returns a summary of all sessions and their states (maybe in JSON).
	•	wa logs <pane_id> [--tail N] – fetch recent logs for a given session.
	•	wa send <pane_id> "<text>" – send text to a session (essentially a wrapper around wezterm cli send-text).
	•	wa switch-account <agent> – triggers an account rotation for the specified agent (this would encapsulate the logout/login flow described).
	•	wa help – quick-start usage information.
If no arguments are given (wa quick start mode), it could output a brief guide of the most important commands and what they do, optimized for an AI agent to quickly learn the interface (token-efficient descriptions, possibly JSON-formatted help).

For example, wa status might output:

{
  "sessions": [
    { "pane_id": 5, "domain": "dev-server", "agent": "Claude", "status": "running", "current_project": "lumera_ai" },
    { "pane_id": 7, "domain": "dev-server", "agent": "Codex", "status": "limit_warning", "remaining": "10%" },
    { "pane_id": 9, "domain": "staging", "agent": "Gemini", "status": "running" }
  ]
}

This tells the agent user which sessions exist, what agent is in each (if identified), and any notable status (like one is nearing limit). The agent could then decide to issue a switch or focus on a particular one.

Alternatively, wa status could return a Markdown table if that’s easier for an AI to parse in some contexts, but JSON is straightforward.

Robustness and Safety
	•	Concurrency: WezTerm CLI commands are lightweight, but if we have many sessions, we should poll responsibly. The list command is cheap; get-text on a pane of moderate size is also fairly cheap (it just dumps text from memory). We will still ensure we don’t call get-text too rapidly. Perhaps adjust frequency based on activity: if a pane is producing output (we detect new lines every poll), we keep polling fast; if it’s idle for a while, we can slow down polling that pane to reduce overhead.
	•	Error Handling: If a wezterm cli command fails (e.g., if WezTerm was closed or a pane disappeared mid-run), wa should handle gracefully – maybe refresh the session list and update its state (remove that pane from monitoring, etc.). If WezTerm is not running at all, wa can either start a WezTerm instance (via wezterm start command) or just warn and exit.
	•	Security: The automation should be careful when sending commands to avoid accidental destructive actions. For instance, ensure that triggered commands (like sending the “Reread AGENTS.md” prompt) only happen in the intended context (after a compaction event in an agent pane, not just because someone typed those words in a code file). Our pattern matching should be specific enough (anchored or with context) to minimize false positives. Additionally, storing logs in SQLite means sensitive info might be recorded – ensure the database file is in a secure location with proper permissions.
	•	Responsiveness: The whole point is to react faster and more reliably than a human could. With wa, reactions happen within maybe 0.5–1 second of the trigger (depending on poll interval). If needed, we can reduce the interval or even use WezTerm’s update-status event as a trigger to poll (since update-status fires roughly once per second by default). WezTerm doesn’t yet have a direct hook “on new output” for remote panes due to PTY limitations ￼, so polling is our method.
	•	Play nice with UI: Our automation should not interfere with manual control. For example, if the user is actively typing in a session, wa might still send automated input – that’s potentially disruptive. One approach is to suppress automated actions if we detect the user is currently active in that pane (WezTerm can tell if a pane is focused, and we might only auto-send if it’s not currently focused by the user, or if a certain mode is enabled). Another approach is to have a “consent” from the agent – since here the “user” might actually be an AI agent controlling it, we assume it’s fine. If a human is using the system, they would know about wa and could pause it if needed.

Example Workflow Automation

Let’s walk through an example scenario combining everything:
	•	Step 0: You launch WezTerm and wa. WezTerm opens your 3 remote windows (Dev, Staging, Workstation). In one tab on Dev, you start a Claude coding session (cc for Claude Code). In another tab on Dev, you start a Codex session (cod for Codex CLI). wa detects new processes (perhaps by noticing lines like “Claude Code v2.1.12” and “Welcome to Codex…” in those panes) and labels pane 5 as Claude, pane 7 as Codex. It begins monitoring them.
	•	Step 1: As you work with Claude, after a while, the conversation gets long and Claude triggers compaction. In the pane 5 output, wa sees “Conversation compacted”. Within a second, wa reacts by sending the prompt to reread a key file. This happens automatically; in the UI, you’ll just see that message appear as if you typed it instantly after compaction. Claude reads the file as instructed, ensuring continuity.
	•	Step 2: Meanwhile, in the Codex session on pane 7, you’ve been coding for a few hours. A warning appears: “⚠ … less than 5% of your limit left.” wa logs it and might output a note on its side-channel (maybe in its own console or status) that Codex is low on time, but it doesn’t intervene yet.
	•	Step 3: Eventually, Codex says “You’ve hit your usage limit… resets at 12:37 AM.” The agent in Codex stops responding to new prompts. wa immediately catches this and proceeds to handle it:
	•	It sends Ctrl+C twice to pane 7 to terminate the Codex session (this causes Codex to print the token usage summary and resume ID).
	•	It parses the resume ID from the output (e.g., 019bcd5e-1de1-7402-a508-b4c57ab6fb62).
	•	It initiates the account switching: since this is Codex, which uses ChatGPT auth, wa uses an available stored account (or the next API key). It runs cod login --device-auth in pane 7. This prints the device code and URL.
	•	wa launches an automated browser process that opens the URL, logs in with the next account’s credentials (which could be pre-saved or provided via a secure method), and enters the device code. Suppose all goes well, after maybe 5-10 seconds, the Codex CLI prints “Successfully logged in”.
	•	wa now sends cod resume 019bcd5e-... to pane 7, which reopens the session exactly where it left off (but under the new account). It then sends a simple “proceed.” or presses Enter to prompt the agent to continue. Codex, now running with fresh quota, continues the conversation.
	•	Throughout, wa logs each of these steps (maybe at least to its own debug log). To the user, this all happened almost automatically – they just see Codex say limit hit, then a flurry of actions and then it resumes. No manual login steps were needed.
	•	Step 4: Later, if the user or AI agent controlling these wants to review what happened, they can use wa to query the log. For instance, wa search "reset your usage limit" could find instances of usage resets. Or they could query all compactions events across sessions to see how often context is being cleared.
	•	Step 5: If something goes wrong (say the login fails because of wrong password or device code expiry), wa can detect lack of success message within a timeout and report an error status for that session (so the agent controlling it knows human intervention is needed).

Additional Tips for Lua Integration (Advanced)

While we focus on the external tool, note that some tasks can also be aided by WezTerm’s Lua if desired:
	•	You could define a custom status indicator for each pane that shows an automation status (like if an account switch is in progress). For example, using wezterm.on("update-right-status", ...) to maybe add a symbol in the tab title if a pane has unseen output or a warning. The has_unseen_output field can signal that a background tab produced output ￼, which could hint that an agent responded or some event happened. Our config above already colors tabs when they are active/inactive; one could extend it to flash a different color if pane.has_unseen_output is true and maybe the output contains a keyword. However, doing heavy text parsing in Lua config isn’t ideal – better to let wa handle it.
	•	Use User Vars or OSC: If we had control over the agent programs, we could make them emit a custom OSC (Operating System Command) sequence to signal events. For instance, the Codex CLI could theoretically output OSC 1337;SetUserVar=LIMIT=hit ST upon hitting limit (if it were programmed to do so). WezTerm would catch that as a user-var-changed event ￼, and we could handle it in Lua without text parsing. But since we cannot easily change these closed-source agents, parsing their plain text is the way.
	•	WezTerm also has an Open URI event (when you Cmd+Click or launch a URL) ￼. If device auth codes come with URLs in output, one could imagine auto-catching them. But our approach is to use an external browser automation, which is fine.

Troubleshooting & Common Pitfalls

Even with careful setup, you may encounter issues. Here are some common ones and how to address them:

Problem / Symptom	Likely Cause	Solution / Fix
Cannot connect to remote domain (WezTerm says “Timed out” or “Connection failed”)	SSH to server failed, or wezterm-mux-server not running on remote, or version mismatch.	Verify you can ssh user@host from terminal. Ensure the remote service is active (systemctl --user status wezterm-mux-server). Check for version mismatches and update if needed. Also ensure your local config’s ssh_domains entries are correct (hostnames, usernames).
Remote sessions don’t persist (get closed when Mac closes)	The mux server likely isn’t running persistently. Perhaps linger not enabled or service not started.	SSH into remote after a disconnect, run systemctl --user status wezterm-mux-server. If it’s not running, enable linger (loginctl enable-linger). If it crashed, check logs. Make sure you used multiplexing = "WezTerm" in config; if you used plain SSH mode, sessions won’t persist.
Too many tabs opening on each launch	The gui-startup logic might be creating new tabs every time instead of reusing.	Check the condition that determines if a remote already had tabs. In our config, we used #tabs <= 1. If you accidentally removed that or always spawn tabs, you’ll duplicate. Also ensure you don’t have multiple gui-startup handlers stacking via config reloads. Only one should run.
Colors or domain-specific config not applying	Possibly the pane:get_domain_name() isn't matching your domain (maybe domain name typo).	Print/debug the domain name to see what it is. It should match one of the keys in domain_colors. Note: wa v0.2.0+ removed the Lua update-status hook; use CLI polling and user-var signaling instead.
Keybindings conflicts	Leader key or others not working as expected.	Make sure no other app (or macOS itself) is intercepting the combo. On macOS, Ctrl+Arrow might be tied to Mission Control; you may need to adjust those OS shortcuts. Also note WezTerm doesn’t allow duplicate key assignments – ensure your config isn’t merging multiple tables causing duplicates. Use wezterm show-keys to see active bindings.
Automation tool (wa) isn’t catching events	The pattern might not match exactly due to formatting differences, or wa might not be polling frequently enough.	Test your regex patterns against sample output to ensure they match (taking into account punctuation, etc.). Increase polling frequency or use a manual trigger (you can always force a read with wa logs pane_id). Also confirm wa has correct pane IDs – they can change if you close and reopen tabs. wa should refresh the list if needed.
Unable to send Ctrl+C or special keys	wezterm cli send-text by itself doesn’t send special keycodes.	Use --no-paste and the literal control character if possible. In bash, ^C is 0x03. You could echo that via printf "\x03". On Linux/macOS, you might also use wezterm cli send-text --pane-id X $'\x03' --no-paste (using shell ANSI C quoting). If that fails, consider sending the escape sequence for SIGINT – in some contexts \x03 should work. Alternatively, as a last resort, ssh into the remote and kill -INT <pid> of the process. But that’s heavier.
WezTerm high CPU usage or slow	Possibly the Lua config doing expensive work (like large text processing) on every frame.	Offload text scanning to the external tool (as we do). If you attempted to parse output in update-status, that could slow things. Also, extremely frequent polling (many times per second) could stress things – 1 Hz to 2 Hz per pane is usually fine. WezTerm itself is quite efficient with render, but if you use very large scrollback (default is 3500 lines; if you increased it to say 50k) and constantly dump it, that’s more data to handle. Tune as needed.
SQLite database growing large	Logging everything can consume space.	Implement log rotation or pruning in wa. For example, you might delete old entries after X days, or use an FTS (full-text search) virtual table with a content limit. If the DB is huge, queries also slow down. So consider only indexing key fields or compressing old logs.
Agent sessions interfering with each other	If wa sends a command to the wrong pane (misidentified agent) or timing issues cause overlap.	Ensure that each pane’s automation is isolated. Use per-pane locks if sending multi-step sequences. Double-check your identification logic (e.g., don’t send a Claude-specific command to a Codex pane). It might be safer to require user to tag a tab (perhaps rename the tab title to include agent name) to positively identify. In absence of that, use multiple pattern checks (Claude’s ASCII art vs Codex’s warnings are quite distinct).

Finally, if you run into issues not covered here, the WezTerm community is active. Check the WezTerm [FAQ/Troubleshooting docs] ￼ and GitHub discussions. Often, problems come down to configuration mistakes or environmental quirks that can be resolved.

Conclusion

By harnessing WezTerm’s modern multiplexing and Lua automation capabilities, we can create a robust environment to manage multiple AI coding agents across various machines as if they were just tabs in one window. WezTerm provides the persistence, performance, and scriptability needed for such advanced workflows ￼, while a dedicated automation tool like wezterm_automaton can serve as the “brain” coordinating all the agents.

This approach offers:
	•	Resilience: Sessions survive network issues and restarts, and usage limits can be handled by swiftly switching contexts or accounts.
	•	Efficiency: No need for clunky expect scripts or manual tmux send-keys – we interface directly with WezTerm’s API for precise control. GPU acceleration and optimized I/O in WezTerm ensure even heavy outputs (like large logs or diff outputs) don’t bog down the experience ￼.
	•	Transparency: Everything is logged. You can query past conversations or events easily, enabling analysis of agent behavior over time (e.g., how often does compaction happen? How many tokens used?).
	•	Extensibility: Adding a new agent type or new automation trigger is as simple as adding another pattern and handler. The same infrastructure can manage any CLI tool running in a terminal.

In summary, WezTerm + Lua + Rust form a powerful trio for building an AI agent management console that is far more capable than a traditional terminal or tmux setup. We’ve covered the full spectrum from initial configuration to the internals of capturing output and reacting to it. With this guide, a coding agent (or a savvy developer) should be equipped to implement wa and tailor it to their needs – ultimately creating a system where AI agents can oversee other AI agents, all orchestrated through WezTerm.

Last Updated: January 2026
