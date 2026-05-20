-- FrankenTerm default GUI config.
--
-- Ships inside FrankenTerm.app at Contents/Resources/frankenterm.lua and is loaded
-- automatically when no user config (~/.frankenterm.lua or
-- ~/.config/frankenterm/{frankenterm,wezterm}.lua) is present.
--
-- This file is intentionally a near-copy of the reference WezTerm config the
-- author uses day-to-day, ported to FrankenTerm so:
--   - the GUI talks to remote frankenterm-mux-server (not wezterm-mux-server),
--     keeping FrankenTerm and WezTerm side-by-side on every host;
--   - Pragmasevka Nerd Font is the default font (the bundled v1.7.0 archive
--     under Contents/Resources/fonts/ supplies all 4 faces);
--   - keybindings, color schemes, leader chord, gui-startup tab seeding, and
--     per-host gradient backgrounds match the user's WezTerm setup so muscle
--     memory carries over.

local frankenterm = require 'frankenterm'
local config = frankenterm.config_builder()
local act = frankenterm.action

-- ============================================================================
-- Helper: is the pane running an interactive shell?
-- ============================================================================
local function is_shell(foreground_process_name)
  local shell_names = { 'bash', 'zsh', 'fish', 'sh', 'ksh', 'dash' }
  local process = string.match(foreground_process_name, '[^/\\]+$') or foreground_process_name
  for _, shell in ipairs(shell_names) do
    if process == shell then
      return true
    end
  end
  return false
end

-- ============================================================================
-- Hyperlink click handler: open dirs in shell, text files in nvim
-- ============================================================================
frankenterm.on('open-uri', function(window, pane, uri)
  local editor = 'nvim'
  if uri:find '^https?:' then
    return true
  end
  if uri:find '^file:' == 1 and not pane:is_alt_screen_active() then
    local url = frankenterm.url.parse(uri)
    if is_shell(pane:get_foreground_process_name()) then
      local success, stdout, _ = frankenterm.run_child_process {
        'file', '--brief', '--mime-type', url.file_path,
      }
      if success then
        if stdout:find 'directory' then
          pane:send_text(frankenterm.shell_join_args { 'cd', url.file_path } .. '\r')
          pane:send_text('ls\r')
          return false
        end
        if stdout:find 'text' then
          if url.fragment then
            pane:send_text(frankenterm.shell_join_args { editor, '+' .. url.fragment, url.file_path } .. '\r')
          else
            pane:send_text(frankenterm.shell_join_args { editor, url.file_path } .. '\r')
          end
          return false
        end
      end
    end
  end
  return true
end)

-- ============================================================================
-- REMOTE DOMAINS — proxy_command over SSH to frankenterm-mux-server
-- ============================================================================
-- FrankenTerm's SSH multiplexing client still invokes `wezterm cli proxy` on
-- the remote (legacy from the fork), so an ssh_domain entry would connect us
-- to the *WezTerm* mux, not the FrankenTerm mux. To talk to the FrankenTerm
-- mux specifically, we use unix_domains with a proxy_command: SSH pipes
-- stdin/stdout to the FrankenTerm socket directly via `nc -U`.
--
-- Each remote runs frankenterm-mux-server as a systemd --user service,
-- listening on $XDG_RUNTIME_DIR/frankenterm/sock (= /run/user/1000/frankenterm/sock).
-- The WezTerm mux on the same host stays on /run/user/1000/wezterm/sock,
-- so the two muxes coexist with zero shared state.
--
-- SSH key paths are absolute because the GUI app's runtime env may not
-- expand ~ the way an interactive shell would.
local ssh_remotes = {
  {
    name           = 'csd',
    remote_address = '144.126.137.164',
    username       = 'ubuntu',
    ssh_key        = frankenterm.home_dir .. '/.ssh/contabo_new_baremetal_sense_demo_box.pem',
    cwd            = '/data/projects',
  },
  {
    name           = 'css',
    remote_address = '209.145.54.164',
    username       = 'ubuntu',
    ssh_key        = frankenterm.home_dir .. '/.ssh/contabo_new_baremetal_superserver_box.pem',
    cwd            = '/data/projects',
  },
  {
    name           = 'trj',
    remote_address = '10.10.10.1',
    username       = 'ubuntu',
    ssh_key        = frankenterm.home_dir .. '/.ssh/trj_ed25519',
    cwd            = '/data/projects',
  },
  {
    name           = 'ts1',
    remote_address = '100.74.254.33',
    username       = 'ubuntu',
    ssh_key        = frankenterm.home_dir .. '/.ssh/thinkstation1_ed25519',
    cwd            = '/data/projects',
  },
  {
    name           = 'ts2',
    remote_address = '100.96.111.98',
    username       = 'ubuntu',
    ssh_key        = frankenterm.home_dir .. '/.ssh/thinkstation2_ed25519',
    cwd            = '/data/projects',
  },
}

local function ft_proxy_command(remote)
  return {
    'ssh',
    '-i', remote.ssh_key,
    '-o', 'ConnectTimeout=10',
    '-o', 'BatchMode=yes',
    '-o', 'ServerAliveInterval=30',
    '-o', 'ServerAliveCountMax=4',
    '-o', 'StrictHostKeyChecking=accept-new',
    remote.username .. '@' .. remote.remote_address,
    -- Use nc -U (netcat-openbsd, standard on Ubuntu) to splice ssh stdio to
    -- the frankenterm mux socket. Hard-coded to UID 1000 because that's the
    -- ubuntu user on every host in the fleet; bake into the per-host entry
    -- below if any host ever runs frankenterm under a different UID.
    'nc -U /run/user/1000/frankenterm/sock',
  }
end

config.unix_domains = {}
for _, r in ipairs(ssh_remotes) do
  table.insert(config.unix_domains, {
    name           = r.name,
    proxy_command  = ft_proxy_command(r),
    -- We are the source of truth for the socket path; we never bind locally.
    skip_permissions_check = true,
    -- 2 min I/O timeout — generous, matches the wezterm default for ssh muxes.
    read_timeout   = 120,
    write_timeout  = 120,
  })
end

-- For the ssh_info / startup helper below we also need the same per-host
-- metadata in a name-keyed table.
local ssh_info = {}
for _, r in ipairs(ssh_remotes) do
  ssh_info[r.name] = { host = r.remote_address, user = r.username, key = r.ssh_key, cwd = r.cwd }
end

-- ============================================================================
-- DOMAIN-AWARE COLOR SCHEMES (host-aware-color-themes pattern)
-- ============================================================================
-- Each remote gets a distinct gradient + cursor + tab-bar palette so a
-- glance at the window/tab bar tells you which host the pane is on.
-- Applied via frankenterm.on('update-status') below.

local domain_colors = {
  csd = { -- Sense Demo: Solar Flare / amber
    background = {{ source = { Gradient = { colors = { '#0a0a06', '#14120a', '#1a1610' }, orientation = { Linear = { angle = -45.0 } } } }, width = '100%', height = '100%', opacity = 0.92 }},
    solid_bg = '#0a0a06', tab_bg = '#ff9933', foreground = '#ffe0b3', cursor = '#ffaa00',
    ansi    = { '#1a1a10', '#ff6644', '#99ff66', '#ffdd44', '#44aaff', '#ff9944', '#44ffdd', '#d5c4a1' },
    brights = { '#3d3d2f', '#ff9977', '#bbff99', '#ffee77', '#77ccff', '#ffbb77', '#77ffee', '#fffaf0' },
  },
  css = { -- Super Server: Ultraviolet / purple
    background = {{ source = { Gradient = { colors = { '#08060e', '#100a18', '#180e22' }, orientation = { Linear = { angle = -45.0 } } } }, width = '100%', height = '100%', opacity = 0.92 }},
    solid_bg = '#08060e', tab_bg = '#aa66ff', foreground = '#e0d0ff', cursor = '#cc44ff',
    ansi    = { '#1a1429', '#ff44aa', '#44ff99', '#ffcc44', '#6699ff', '#cc66ff', '#44ddff', '#c7c0e0' },
    brights = { '#3d3450', '#ff77cc', '#77ffbb', '#ffdd77', '#99bbff', '#dd99ff', '#77eeff', '#f0e8ff' },
  },
  trj = { -- Threadripper: Cyberpunk Aurora / magenta+cyan
    background = {{ source = { Gradient = { colors = { '#0a0e14', '#0e1420', '#12182a' }, orientation = { Linear = { angle = -45.0 } } } }, width = '100%', height = '100%', opacity = 0.92 }},
    solid_bg = '#0a0e14', tab_bg = '#ff00ff', foreground = '#b3f4ff', cursor = '#ff00ff',
    ansi    = { '#1a1f29', '#ff3366', '#39ffb4', '#ffe566', '#00aaff', '#ff00ff', '#00ffff', '#c7d5e0' },
    brights = { '#3d4f5f', '#ff6b9d', '#6bffcd', '#ffef99', '#66ccff', '#ff66ff', '#66ffff', '#ffffff' },
  },
  ts1 = { -- ThinkStation1: Teal Forge
    background = {{ source = { Gradient = { colors = { '#040e0e', '#081818', '#0c2222' }, orientation = { Linear = { angle = -45.0 } } } }, width = '100%', height = '100%', opacity = 0.92 }},
    solid_bg = '#040e0e', tab_bg = '#00ccaa', foreground = '#b0ffe0', cursor = '#00ffcc',
    ansi    = { '#0a1a1a', '#ff5566', '#00ffaa', '#ddff44', '#44aaff', '#bb66ff', '#00ffdd', '#a0d5c0' },
    brights = { '#2a3a3a', '#ff8899', '#66ffcc', '#eeff77', '#77ccff', '#dd99ff', '#66ffee', '#d0fff0' },
  },
  ts2 = { -- ThinkStation2: Coral Reef
    background = {{ source = { Gradient = { colors = { '#0e0404', '#180808', '#220c0c' }, orientation = { Linear = { angle = -45.0 } } } }, width = '100%', height = '100%', opacity = 0.92 }},
    solid_bg = '#0e0404', tab_bg = '#ff6655', foreground = '#ffb0a0', cursor = '#ff7766',
    ansi    = { '#1a0a0a', '#ff5566', '#66ffaa', '#ffdd44', '#44aaff', '#ff6699', '#44ffdd', '#c0a0a0' },
    brights = { '#3a2a2a', '#ff8899', '#99ffcc', '#ffee77', '#77ccff', '#ff99bb', '#77ffee', '#ffe0d0' },
  },
}

local domain_info = {
  csd = { name = 'Sense Demo',    icon = '󰒋 ' },
  css = { name = 'Super Server',  icon = '󰒋 ' },
  trj = { name = 'Threadripper',  icon = '󰻠 ' },
  ts1 = { name = 'ThinkStation1', icon = '󰻠 ' },
  ts2 = { name = 'ThinkStation2', icon = '󰻠 ' },
}

-- Pre-build status strings + overrides ONCE at config load so
-- update-status (which fires several times/sec per window) is allocation-
-- free. Building frankenterm.format tables in the handler caused 14G+ Lua GC
-- churn under the previous live config — keep the lookup cheap.
local right_status_by_domain = {}
local overrides_by_domain = {}
for domain, info in pairs(domain_info) do
  local dc = domain_colors[domain]
  right_status_by_domain[domain] = frankenterm.format {
    { Foreground = { Color = '#0d0d1a' } },
    { Background = { Color = dc and dc.tab_bg or '#7aa2f7' } },
    { Attribute = { Intensity = 'Bold' } },
    { Text = ' ' .. info.icon .. info.name .. ' ' },
  }
  if dc then
    overrides_by_domain[domain] = {
      background = dc.background,
      colors = {
        background = dc.solid_bg,
        foreground = dc.foreground,
        cursor_bg = dc.cursor,
        cursor_fg = dc.solid_bg,
        selection_bg = 'rgba(255, 255, 255, 0.2)',
        selection_fg = '#ffffff',
        ansi = dc.ansi,
        brights = dc.brights,
        tab_bar = {
          background = dc.solid_bg,
          active_tab = { bg_color = dc.tab_bg, fg_color = '#000000', intensity = 'Bold' },
          inactive_tab = { bg_color = dc.solid_bg, fg_color = dc.foreground },
        },
      },
    }
  end
end

local leader_left_status = frankenterm.format {
  { Foreground = { Color = '#0d0d1a' } },
  { Background = { Color = '#9ece6a' } },
  { Attribute = { Intensity = 'Bold' } },
  { Text = '  LEADER ' },
}

local window_state = {}
frankenterm.on('update-status', function(window, pane)
  local ok, domain = pcall(function() return pane:get_domain_name() end)
  if not ok then return end
  local window_id = window:window_id()
  local state = window_state[window_id]
  if not state then
    state = { domain = false, right = false, left = false }
    window_state[window_id] = state
  end

  if state.domain ~= domain then
    state.domain = domain
    local overrides = overrides_by_domain[domain]
    if overrides then
      window:set_config_overrides(overrides)
    else
      local cur = window:get_config_overrides() or {}
      if cur.background ~= nil or cur.colors ~= nil then
        cur.background = nil
        cur.colors = nil
        window:set_config_overrides(cur)
      end
    end
    local right = right_status_by_domain[domain] or ''
    if state.right ~= right then
      state.right = right
      window:set_right_status(right)
    end
  end

  local left = window:leader_is_active() and leader_left_status or ''
  if state.left ~= left then
    state.left = left
    window:set_left_status(left)
  end
end)

-- ============================================================================
-- LEADER + KEYBINDINGS
-- ============================================================================
config.leader = { key = 'a', mods = 'CTRL', timeout_milliseconds = 1000 }
config.status_update_interval = 5000

config.keys = {
  { key = 'w', mods = 'LEADER', action = act.ShowLauncherArgs { flags = 'FUZZY|DOMAINS|WORKSPACES' } },
  { key = '1', mods = 'LEADER', action = act.SpawnCommandInNewTab { domain = { DomainName = 'csd' } } },
  { key = '2', mods = 'LEADER', action = act.SpawnCommandInNewTab { domain = { DomainName = 'css' } } },
  { key = '3', mods = 'LEADER', action = act.SpawnCommandInNewTab { domain = { DomainName = 'trj' } } },
  { key = '4', mods = 'LEADER', action = act.SpawnCommandInNewTab { domain = { DomainName = 'ts1' } } },
  { key = '5', mods = 'LEADER', action = act.SpawnCommandInNewTab { domain = { DomainName = 'ts2' } } },
  { key = 'NumLock',        mods = 'CTRL', action = act.SpawnCommandInNewTab { domain = { DomainName = 'local' }, cwd = frankenterm.home_dir .. '/projects' } },
  { key = 'KeypadDivide',   mods = 'CTRL', action = act.SpawnCommandInNewTab { domain = { DomainName = 'css' }, cwd = '/data/projects' } },
  { key = 'KeypadMultiply', mods = 'CTRL', action = act.SpawnCommandInNewTab { domain = { DomainName = 'csd' }, cwd = '/data/projects' } },
  { key = 'KeypadSubtract', mods = 'CTRL', action = act.SpawnCommandInNewTab { domain = { DomainName = 'trj' }, cwd = '/data/projects' } },
  { key = 'LeftArrow',  mods = 'SHIFT|CTRL',  action = act.ActivateTabRelative(-1) },
  { key = 'RightArrow', mods = 'SHIFT|CTRL',  action = act.ActivateTabRelative(1) },
  { key = 'PageUp',     mods = 'CTRL|SHIFT',  action = act.MoveTabRelative(-1) },
  { key = 'PageDown',   mods = 'CTRL|SHIFT',  action = act.MoveTabRelative(1) },
}

-- ============================================================================
-- FONT + WINDOW APPEARANCE
-- ============================================================================
-- Pragmasevka is bundled at Contents/Resources/fonts/ inside the .app bundle
-- and registered automatically via prepend_bundled_app_font_dirs() in
-- frankenterm/config/src/config.rs. The four faces (Regular/Bold/Italic/
-- BoldItalic) come from the v1.7.0 zstd-packed payload at
-- crates/frankenterm/assets/Pragmasevka_NF.zip.zst.
config.font = frankenterm.font {
  family = 'Pragmasevka Nerd Font',
  harfbuzz_features = { 'calt', 'clig', 'liga' },
}
config.font_size = 16.0
config.warn_about_missing_glyphs = false

-- Window translucency: 0.95 (was 0.85) so the effective alpha with the
-- gradient layer (0.92) is ~0.87. A bit of desktop bleed-through for
-- the glassy look, but enough opacity that text on a busy wallpaper
-- stays legible. The blur smooths whatever does show through.
config.window_background_opacity = 0.95
config.macos_window_background_blur = 30
config.background = {
  {
    source = { Gradient = { colors = { '#060a10', '#0a1018', '#0e1420' }, orientation = { Linear = { angle = -45.0 } } } },
    width = '100%', height = '100%', opacity = 0.92,
  },
}

-- Local-domain color scheme: Neon Blue
config.colors = {
  foreground = '#b0e0ff',
  background = '#060a10',
  cursor_bg = '#00aaff',
  cursor_fg = '#060a10',
  cursor_border = '#00aaff',
  selection_fg = '#ffffff',
  selection_bg = 'rgba(0, 170, 255, 0.3)',
  scrollbar_thumb = '#1a2530',
  split = '#1a3050',
  ansi = { '#1a2030', '#ff5577', '#44ffaa', '#ffcc44', '#00aaff', '#aa77ff', '#00ddff', '#b0c4de' },
  brights = { '#3a4050', '#ff8899', '#77ffcc', '#ffdd77', '#55ccff', '#cc99ff', '#55eeff', '#e0f0ff' },
  tab_bar = {
    background = '#060a10',
    active_tab = { bg_color = '#00aaff', fg_color = '#000000', intensity = 'Bold' },
    inactive_tab = { bg_color = '#0a1420', fg_color = '#6090b0', intensity = 'Half' },
    inactive_tab_hover = { bg_color = '#1a2530', fg_color = '#00aaff' },
    new_tab = { bg_color = '#0a1420', fg_color = '#6090b0' },
    new_tab_hover = { bg_color = '#1a2530', fg_color = '#00aaff' },
  },
}

config.window_frame = {
  font = frankenterm.font { family = 'Pragmasevka Nerd Font', weight = 'Bold' },
  font_size = 12.0,
  active_titlebar_bg = '#060a10',
  inactive_titlebar_bg = '#060a10',
}

config.window_padding = { left = 12, right = 12, top = 12, bottom = 12 }
config.inactive_pane_hsb = { saturation = 0.85, brightness = 0.7 }
config.default_cursor_style = 'BlinkingBar'
config.cursor_blink_rate = 500
config.window_decorations = 'RESIZE'

config.use_fancy_tab_bar = true
config.tab_bar_at_bottom = false
config.hide_tab_bar_if_only_one_tab = false
config.tab_max_width = 120
config.show_tab_index_in_tab_bar = true

config.window_close_confirmation = 'NeverPrompt'
config.skip_close_confirmation_for_processes_named = {
  'bash', 'sh', 'zsh', 'fish', 'tmux', 'nu', 'ssh',
  'wezterm-mux-server', 'frankenterm-mux-server',
  'claude', 'node', 'python', 'python3',
}

config.hyperlink_rules = frankenterm.default_hyperlink_rules()

config.mouse_bindings = {
  { event = { Up   = { streak = 1, button = 'Left'   } }, mods = 'CMD',  action = act.OpenLinkAtMouseCursor },
  { event = { Down = { streak = 1, button = 'Right'  } }, mods = 'NONE', action = act.PasteFrom('Clipboard') },
  { event = { Down = { streak = 1, button = 'Middle' } }, mods = 'NONE', action = act.PasteFrom('PrimarySelection') },
}

-- Performance: agent-swarm-tuned defaults (mirror of the local wezterm setup).
config.scrollback_lines = 100000
config.mux_output_parser_buffer_size = 512 * 1024
config.mux_output_parser_coalesce_delay_ms = 3

-- ============================================================================
-- GUI STARTUP: one local window + one window per remote frankenterm mux
-- ============================================================================
-- 1. Local window with 3 tabs at ~/projects.
-- 2. For each remote domain: attach to the frankenterm mux. If the mux is
--    empty (fresh systemd start), seed 3 tabs at /data/projects. If the mux
--    already holds windows from a previous session, we just inherit them —
--    no tab accumulation across launches.
-- 3. Each remote attach is staggered by 1s so they don't fight for SSH /
--    network bandwidth at startup.
local startup_domains = {
  { name = 'css', cwd = '/data/projects', tabs = 3 },
  { name = 'csd', cwd = '/data/projects', tabs = 3 },
  { name = 'trj', cwd = '/data/projects', tabs = 3 },
  { name = 'ts1', cwd = '/data/projects', tabs = 3 },
  { name = 'ts2', cwd = '/data/projects', tabs = 3 },
}

-- The reference wezterm config consolidates remote mux windows pre-attach via
-- `wezterm cli list` + `wezterm cli move-pane-to-new-tab`. Frankenterm doesn't
-- ship an equivalent `ft cli` mux-control surface yet, so the consolidation
-- step is a no-op here — if a remote mux has multiple windows we will surface
-- them all on attach. Track as a future improvement when `ft cli` lands a
-- mux-control verb (see beads ft-mux-cli-parity).

local function domain_has_panes(domain_name)
  for _, w in ipairs(frankenterm.mux.all_windows()) do
    for _, t in ipairs(w:tabs()) do
      for _, p in ipairs(t:panes()) do
        local ok, dname = pcall(function() return p:get_domain_name() end)
        if ok and dname == domain_name then
          return true
        end
      end
    end
  end
  return false
end

frankenterm.on('gui-startup', function(cmd)
  if cmd and cmd.args and #cmd.args > 0 then
    local tab, pane, window = frankenterm.mux.spawn_window(cmd)
    if window:gui_window() then window:gui_window():maximize() end
    return
  end

  -- 1) Local window: 3 tabs in ~/projects
  local _, _, window = frankenterm.mux.spawn_window({ cwd = frankenterm.home_dir .. '/projects' })
  for _ = 2, 3 do
    window:spawn_tab({ cwd = frankenterm.home_dir .. '/projects' })
  end
  local gui = window:gui_window()
  if gui then gui:maximize() end

  -- 2) Remote domains: staggered attach with frankenterm.time.call_after so
  --    we don't open five SSH connections in the same event-loop tick. Each
  --    remote gets its own 1-second offset before its attach fires; if the
  --    remote mux is empty we seed N tabs, otherwise we inherit whatever the
  --    persistent mux already had.
  --
  --    `frankenterm.time.call_after(secs, fn)` is the FrankenTerm-policy-
  --    compliant timer (asupersync-backed under the hood; project bans
  --    direct tokio use — see AGENTS.md). The callback runs on the main
  --    thread with full access to the Lua state.
  for idx, dcfg in ipairs(startup_domains) do
    frankenterm.time.call_after(idx * 1.0, function()
      local info = ssh_info[dcfg.name]
      if not info then return end
      local ok, err = pcall(function()
        local domain = frankenterm.mux.get_domain(dcfg.name)
        if not domain then
          frankenterm.log_error('gui-startup: domain not registered: ' .. dcfg.name)
          return
        end
        frankenterm.log_info('gui-startup: attaching to domain ' .. dcfg.name)
        domain:attach()
        if not domain_has_panes(dcfg.name) then
          frankenterm.log_info('gui-startup: ' .. dcfg.name .. ' is empty; seeding ' .. (dcfg.tabs or 3) .. ' tabs')
          local seed_ok, seed_err = pcall(function()
            local _, _, rwindow = frankenterm.mux.spawn_window({
              domain = { DomainName = dcfg.name },
              cwd = dcfg.cwd,
            })
            for _ = 2, (dcfg.tabs or 3) do
              rwindow:spawn_tab({ cwd = dcfg.cwd })
            end
            local rgui = rwindow:gui_window()
            if rgui then rgui:maximize() end
          end)
          if not seed_ok then
            frankenterm.log_error('gui-startup: failed to seed tabs in ' .. dcfg.name .. ': ' .. tostring(seed_err))
          end
        else
          frankenterm.log_info('gui-startup: ' .. dcfg.name .. ' already has panes from previous session; inheriting')
        end
      end)
      if not ok then
        frankenterm.log_error('gui-startup: failed to attach ' .. dcfg.name .. ': ' .. tostring(err))
      end
    end)
  end
end)

return config
