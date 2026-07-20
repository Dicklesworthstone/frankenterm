-- FrankenTerm default GUI config.
--
-- Ships inside FrankenTerm.app at Contents/Resources/frankenterm.lua and is loaded
-- automatically when no user config (~/.frankenterm.lua or
-- ~/.config/frankenterm/{frankenterm,wezterm}.lua) is present.
--
-- POLICY: this bundled fallback is generic and fully local. It defines no
-- remote hosts, opens no network connections, and touches nothing outside
-- the local machine. A clean install starts a single local window and does
-- nothing else until the user explicitly configures remote domains in their
-- own config file (see the commented example near the bottom).

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
-- LEADER + KEYBINDINGS
-- ============================================================================
config.leader = { key = 'a', mods = 'CTRL', timeout_milliseconds = 1000 }
config.status_update_interval = 5000

config.keys = {
  { key = 'w', mods = 'LEADER', action = act.ShowLauncherArgs { flags = 'FUZZY|DOMAINS|WORKSPACES' } },
  { key = 'LeftArrow',  mods = 'SHIFT|CTRL',  action = act.ActivateTabRelative(-1) },
  { key = 'RightArrow', mods = 'SHIFT|CTRL',  action = act.ActivateTabRelative(1) },
  { key = 'PageUp',     mods = 'CTRL|SHIFT',  action = act.MoveTabRelative(-1) },
  { key = 'PageDown',   mods = 'CTRL|SHIFT',  action = act.MoveTabRelative(1) },
}

-- ============================================================================
-- LEADER indicator in the left status area
-- ============================================================================
local leader_left_status = frankenterm.format {
  { Foreground = { Color = '#0d0d1a' } },
  { Background = { Color = '#9ece6a' } },
  { Attribute = { Intensity = 'Bold' } },
  { Text = '  LEADER ' },
}

local window_state = {}
frankenterm.on('update-status', function(window, pane)
  local window_id = window:window_id()
  local left = window:leader_is_active() and leader_left_status or ''
  if window_state[window_id] ~= left then
    window_state[window_id] = left
    window:set_left_status(left)
  end
end)

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

-- Window translucency: 0.95 so the effective alpha with the gradient layer
-- (0.92) is ~0.87. A bit of desktop bleed-through for the glassy look, but
-- enough opacity that text on a busy wallpaper stays legible. The blur
-- smooths whatever does show through.
config.window_background_opacity = 0.95
config.macos_window_background_blur = 30
config.background = {
  {
    source = { Gradient = { colors = { '#060a10', '#0a1018', '#0e1420' }, orientation = { Linear = { angle = -45.0 } } } },
    width = '100%', height = '100%', opacity = 0.92,
  },
}

-- Default color scheme: Neon Blue
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

-- Performance: agent-swarm-tuned defaults.
config.scrollback_lines = 100000
config.mux_output_parser_buffer_size = 512 * 1024
config.mux_output_parser_coalesce_delay_ms = 3

-- Renderer: Metal-backed WebGpu, NOT legacy OpenGL. On Apple Silicon the OpenGL
-- backend (_NSOpenGLViewBackingLayer) causes multi-second/minute render stalls
-- under heavy multi-pane agent-swarm output; WebGpu eliminates them.
config.front_end = "WebGpu"
config.webgpu_power_preference = "HighPerformance"

-- ============================================================================
-- GUI STARTUP: one local window, nothing else
-- ============================================================================
-- The bundled default performs NO remote attaches and NO network activity.
frankenterm.on('gui-startup', function(cmd)
  local _, _, window = frankenterm.mux.spawn_window(cmd or {})
  local gui = window:gui_window()
  if gui then gui:maximize() end
end)

-- ============================================================================
-- OPTIONAL: connecting to a remote frankenterm-mux-server
-- ============================================================================
-- Remote domains are strictly opt-in and belong in YOUR user config
-- (~/.frankenterm.lua or ~/.config/frankenterm/frankenterm.lua), never in
-- this bundled default. A unix_domain with an SSH proxy_command splices the
-- GUI to a frankenterm-mux-server socket on the remote host, e.g.:
--
--   config.unix_domains = {
--     {
--       name = 'myhost',
--       proxy_command = {
--         'ssh',
--         '-i', frankenterm.home_dir .. '/.ssh/id_ed25519',
--         '-o', 'ConnectTimeout=10',
--         '-o', 'BatchMode=yes',
--         'user@myhost.example.com',        -- placeholder: your own host
--         'nc -U /run/user/1000/frankenterm/sock',
--       },
--       skip_permissions_check = true,
--       read_timeout = 120,
--       write_timeout = 120,
--     },
--   }
--
-- Attach with the launcher (LEADER+w) or `frankenterm connect myhost`.
-- If you want tabs seeded automatically at startup, do that from your own
-- gui-startup handler in your user config -- the bundled default will never
-- initiate a connection on its own.

return config
