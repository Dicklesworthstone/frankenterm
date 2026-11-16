-- GUI config for tests/e2e/test_gui_lua_timer_reload_macos.sh.
--
-- gui-startup starts ONE self-rescheduling time.call_after chain, the way an
-- operator profile keeps remote domains alive with a watchdog. Each tick
-- appends a line to $FT_TR_HOME/ticks; each evaluation of this file appends a
-- line to $FT_TR_HOME/evals, so the driver can see the reloads it forces by
-- editing this file. gui-startup does not refire on reload, so the chain must
-- carry on from the generation that started it: one tick a second, neither
-- lost nor multiplied.
local ft = require 'frankenterm'
local config = ft.config_builder()
local H = os.getenv('FT_TR_HOME')

-- Never touch a real profile.
if not H or ft.home_dir ~= H then
  os.exit(3)
end

local function append(name, line)
  local f = io.open(H .. '/' .. name, 'a')
  if f then
    f:write(line, '\n')
    f:close()
  end
end

append('evals', tostring(os.time()))

config.check_for_updates = false
config.window_close_confirmation = 'NeverPrompt'
config.automatically_reload_config = true
config.initial_cols = 30
config.initial_rows = 2
config.window_decorations = 'NONE'

local function tick()
  append('ticks', tostring(os.time()))
  ft.time.call_after(1, tick)
end

ft.on('gui-startup', function()
  ft.mux.spawn_window {
    args = { '/bin/sh', '-c', 'printf idle; exec sleep 100000' },
    position = {
      x = tonumber(os.getenv('FT_TR_X') or '40'),
      y = tonumber(os.getenv('FT_TR_Y') or '40'),
      origin = 'MainScreen',
    },
  }
  tick()
end)

return config
-- The driver appends reload markers below this line.
