local ft = require 'frankenterm'
local c = ft.config_builder()
c.check_for_updates = false
c.window_close_confirmation = 'NeverPrompt'
c.automatically_reload_config = false
-- Force atlas rebuilds: start at 128 px and draw large glyphs in a large
-- window, so the atlas-growth corpus outgrows the atlas repeatedly.
c.glyph_atlas_initial_size = 128
c.font_size = 24
c.initial_cols = 140
c.initial_rows = 45
ft.on('gui-startup', function(cmd)
  ft.mux.spawn_window(cmd or {})
  ft.time.call_after(1, function()
    for _, w in ipairs(ft.gui.gui_windows()) do
      w:set_position(640, 1120)
    end
  end)
end)
return c
