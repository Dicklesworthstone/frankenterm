-- GUI config for tests/e2e/test_gui_keeps_windows_across_mux_restart_macos.sh.
--
-- The driver steers this config through $FT_KW_DIR/phase and reads what the
-- GUI shows from $FT_KW_DIR/gui.log, one line per status update of a window
-- showing the e2e domain:
--   phase|<mux window id>|<gui window count>|<tab id>:<pane id>:<title>,...
-- The ids are the GUI's own: a domain that detaches and reattaches gets new
-- ones, so equal ids across a mux restart mean the GUI kept its objects.
local ft = require 'frankenterm'
local config = ft.config_builder()
local E = os.getenv('FT_KW_DIR')
local H = os.getenv('FT_KW_HOME')
local SOCK = os.getenv('FT_KW_SOCK')

-- Never touch a real profile: the layout store and remembered domains live
-- under HOME.
if not E or not H or not SOCK or ft.home_dir ~= H then
  os.exit(3)
end

-- A plain local socket, as the GUI reaches a launchd mux: no proxy_command.
config.unix_domains = {
  { name = 'e2e', socket_path = SOCK, no_serve_automatically = true, skip_permissions_check = true },
}
config.initial_cols = 50
config.initial_rows = 8
config.window_close_confirmation = 'NeverPrompt'
config.automatically_reload_config = false
config.status_update_interval = 400
config.check_for_updates = false

local function read1(p)
  local f = io.open(p, 'r')
  if not f then return nil end
  local s = f:read('*l')
  f:close()
  return s
end
local function touch(p) local f = io.open(p, 'w'); f:write('x'); f:close() end
local function exists(p) return read1(p) ~= nil end

local function tab_cmd(n)
  return { '/bin/sh', '-c', "printf '\\033]0;" .. n .. "\\007'; i=0; while :; do echo " .. n .. " tick $i; i=$((i+1)); sleep 1; done" }
end

ft.on('update-status', function(window, pane)
  if pane:get_domain_name() ~= 'e2e' then
    return
  end
  local phase = read1(E .. '/phase') or 'none'
  local mw = window:mux_window()
  local tabs = {}
  for _, tab in ipairs(mw:tabs()) do
    local p = tab:active_pane()
    table.insert(tabs, tab:tab_id() .. ':' .. p:pane_id() .. ':' .. p:get_title())
  end
  local log = io.open(E .. '/gui.log', 'a')
  log:write(phase .. '|' .. mw:window_id() .. '|' .. #ft.gui.gui_windows() .. '|' .. table.concat(tabs, ',') .. '\n')
  log:close()

  if phase == 'spawn' and not exists(E .. '/spawned') then
    touch(E .. '/spawned')
    for _, n in ipairs { 'B', 'C' } do
      mw:spawn_tab { args = tab_cmd(n), cwd = '/' }
    end
  elseif phase == 'more' and not exists(E .. '/more-spawned') then
    touch(E .. '/more-spawned')
    mw:spawn_tab { args = tab_cmd('D'), cwd = '/' }
  end
end)

return config
