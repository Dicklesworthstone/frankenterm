-- GUI config for tests/e2e/test_gui_repaint_rate_macos.sh.
--
-- FT_RR_MODE picks what the window shows:
--   visible        the visible tab prints 250 lines/s
--   hidden_output  the visible tab is idle; a background tab prints 250 lines/s
--   hidden_title   the visible tab is idle; a background tab changes its title
--                  50 times a second (an agent's spinner)
--   gradient       a maximized window with a full-size gradient background
-- FT_RR_UNFOCUSED sets unfocused_max_fps.
local ft = require 'frankenterm'
local config = ft.config_builder()
local H = os.getenv('FT_RR_HOME')
local MODE = os.getenv('FT_RR_MODE') or 'visible'

-- Never touch a real profile.
if not H or ft.home_dir ~= H then
  os.exit(3)
end

config.check_for_updates = false
config.window_close_confirmation = 'NeverPrompt'
config.automatically_reload_config = false
config.max_fps = 30
config.window_background_opacity = 1.0
local unfocused = os.getenv('FT_RR_UNFOCUSED')
if unfocused then
  config.unfocused_max_fps = tonumber(unfocused)
end

local printing = { '/usr/bin/perl', '-e', '$|=1; my $i=0; while(1){ print $i++, "\\n"; select(undef,undef,undef,0.004) }' }
local idle = { '/bin/sh', '-c', "printf idle; exec sleep 100000" }
local spinner = { '/bin/sh', '-c', "i=0; while true; do i=$((i+1)); printf '\\033]0;spin %d\\007' $i; sleep 0.02; done" }

if MODE == 'gradient' then
  config.background = { {
    source = { Gradient = { colors = { '#0a0e14', '#0e1420', '#12182a' }, orientation = { Linear = { angle = -45.0 } } } },
    width = '100%', height = '100%', opacity = 1.0,
  } }
else
  -- A small undecorated window the driver places where nothing covers it.
  config.initial_cols = 30
  config.initial_rows = 2
  config.window_decorations = 'NONE'
end

ft.on('gui-startup', function()
  if MODE == 'gradient' then
    local _, _, window = ft.mux.spawn_window { args = idle }
    ft.time.call_after(1, function() window:gui_window():maximize() end)
    return
  end
  local position = {
    x = tonumber(os.getenv('FT_RR_X') or '40'),
    y = tonumber(os.getenv('FT_RR_Y') or '40'),
    origin = 'MainScreen',
  }
  if MODE == 'visible' then
    ft.mux.spawn_window { args = printing, position = position }
  else
    local tab, _, window = ft.mux.spawn_window { args = idle, position = position }
    window:spawn_tab { args = (MODE == 'hidden_title') and spinner or printing }
    tab:activate()
  end
end)

return config
