local ft = require 'frankenterm'
local c = ft.config_builder()
c.check_for_updates = false
c.window_close_confirmation = 'NeverPrompt'
c.automatically_reload_config = false
-- Keep the soak window in the screen strip no other window covers, so it is
-- never occluded (and parked) while the workload runs.
ft.on('gui-startup', function(cmd)
  ft.mux.spawn_window(cmd or {})
  ft.time.call_after(1, function()
    for _, w in ipairs(ft.gui.gui_windows()) do
      w:set_position(80, 1120)
    end
  end)
end)
return c
