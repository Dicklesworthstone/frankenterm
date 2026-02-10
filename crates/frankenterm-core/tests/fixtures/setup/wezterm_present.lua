local wezterm = require 'wezterm'
local config = {}

-- FT-BEGIN (do not edit this block)
-- Forward user-var events to ft daemon
wezterm.on('user-var-changed', function(window, pane, name, value)
  if name:match('^ft%-') then
    wezterm.background_child_process {
      'ft', 'event', '--from-uservar',
      '--pane', tostring(pane:pane_id()),
      '--name', name,
      '--value', value
    }
  end
end)
-- FT-END

return config
