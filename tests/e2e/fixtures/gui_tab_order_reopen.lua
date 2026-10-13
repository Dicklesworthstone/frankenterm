-- GUI config for tests/e2e/test_gui_tab_order_reopen_macos.sh.
--
-- The driver steers this config through the file $FT_TO_DIR/phase and reads
-- what the GUI shows from $FT_TO_DIR/order.log ("phase|tab titles|active").
-- Only the e2e domain's window is logged.
local ft = require 'frankenterm'
local act = ft.action
local config = ft.config_builder()
local E = os.getenv('FT_TO_DIR')
local H = os.getenv('FT_TO_HOME')

-- Never touch a real profile: the layout store and remembered domains live
-- under HOME.
if not E or not H or ft.home_dir ~= H then
  os.exit(3)
end

local proxy = { '/usr/bin/nc', '-U', E .. '/mux/.local/share/frankenterm/sock' }
if os.getenv('FT_TO_PROXY') then
  proxy = { '/bin/sh', '-c', os.getenv('FT_TO_PROXY') }
end
config.unix_domains = {
  { name = 'e2e', proxy_command = proxy, skip_permissions_check = true },
}
config.initial_cols = 50
config.initial_rows = 8
config.window_close_confirmation = 'NeverPrompt'
config.automatically_reload_config = false
config.status_update_interval = 400
config.check_for_updates = false

-- FT_TO_LUA_ATTACH=1: start up the way the author's config does: a local
-- window first, then the remote domain attached from Lua on a timer (on top of
-- remembered-domain auto-connect).
if os.getenv('FT_TO_LUA_ATTACH') == '1' then
  ft.on('gui-startup', function(cmd)
    if cmd and cmd.args and #cmd.args > 0 then
      ft.mux.spawn_window(cmd)
      return
    end
    ft.mux.spawn_window {}
    ft.time.call_after(1.0, function()
      local domain = ft.mux.get_domain('e2e')
      if domain and domain:state() ~= 'Attached' then
        domain:attach()
      end
    end)
  end)
end

local function read1(p)
  local f = io.open(p, 'r')
  if not f then return nil end
  local s = f:read('*l')
  f:close()
  return s
end
local function touch(p) local f = io.open(p, 'w'); f:write('x'); f:close() end
local function exists(p) return read1(p) ~= nil end

local function order_of(mw, window)
  local names, active = {}, '?'
  local at = window:active_tab()
  for _, tab in ipairs(mw:tabs()) do
    local t = tab:active_pane():get_title()
    table.insert(names, t)
    if at and tab:tab_id() == at:tab_id() then active = t end
  end
  return table.concat(names, ','), active
end

local function tab_cmd(n)
  return { '/bin/sh', '-c', "sleep 0.5; printf '\\033]0;" .. n .. "\\007'; exec sleep 100000" }
end

local last_action = 0
local first_stale = {}
ft.on('update-status', function(window, pane)
  if pane:get_domain_name() ~= 'e2e' then
    return
  end
  local phase = read1(E .. '/phase') or 'none'
  local mw = window:mux_window()
  local order, active = order_of(mw, window)
  local log = io.open(E .. '/order.log', 'a')
  log:write(phase .. '|' .. order .. '|' .. active .. '\n')
  log:close()

  -- A freshly spawned remote tab can keep its default title until the pane
  -- prints again (ft-pzu8b). Make it print so tabs stay identifiable.
  for _, tab in ipairs(mw:tabs()) do
    local p = tab:active_pane()
    if p:get_title() == 'frankenterm' then
      local id = p:pane_id()
      first_stale[id] = first_stale[id] or os.time()
      if os.time() - first_stale[id] >= 3 then
        first_stale[id] = os.time()
        p:send_text('\r')
      end
    end
  end

  if phase == 'spawn' and not exists(E .. '/spawned') then
    touch(E .. '/spawned')
    for _, n in ipairs { 'B', 'C', 'D' } do
      -- cwd '/': the GUI's own cwd may not exist on a remote mux host.
      mw:spawn_tab { args = tab_cmd(n), cwd = '/' }
    end
  elseif phase == 'move' and os.getenv('FT_TO_SPACED') == '1' and os.time() - last_action < 2 then
    -- Spaced mode: at most one action per 2 s, like deliberate keypresses.
  elseif phase == 'move' then
    -- From A,B,C,D (active D): D to the front, then C one step left, ending
    -- at D,A,C,B with C active. Default mode fires these back to back, like
    -- key repeat.
    last_action = os.time()
    if order == 'A,B,C,D' and active == 'D' then
      window:perform_action(act.MoveTab(0), pane)
    elseif order == 'D,A,B,C' and active ~= 'C' then
      window:perform_action(act.ActivateTab(3), pane)
    elseif order == 'D,A,B,C' and active == 'C' then
      window:perform_action(act.MoveTabRelative(-1), pane)
    end
  elseif phase:sub(1, 4) == 'quit' and not exists(E .. '/' .. phase .. '-sent') then
    touch(E .. '/' .. phase .. '-sent')
    window:perform_action(act.QuitApplication, pane)
  end
end)

return config
