-- GUI config for tests/e2e/test_gui_background_tab_titles_macos.sh.
--
-- FT_BG_ROLE picks what this GUI does on its remote window:
--   setup    create two tabs whose program sets the title to tick-N once a
--            second, go back to the first (idle) tab, then do nothing
--   observe  record every tab's title twice a second, never changing tabs
--   both     setup, then observe
-- FT_BG_SOCK is the mux socket; FT_BG_PROXY replaces the proxy command.
local ft = require 'frankenterm'
local config = ft.config_builder()
local H = os.getenv('FT_BG_HOME')
local ROLE = os.getenv('FT_BG_ROLE') or 'observe'

-- Never touch a real profile.
if not H or ft.home_dir ~= H then
  os.exit(3)
end

config.check_for_updates = false
config.window_close_confirmation = 'NeverPrompt'
config.automatically_reload_config = false
config.status_update_interval = 500

local proxy = os.getenv('FT_BG_PROXY')
config.unix_domains = { {
  name = 'e2e',
  proxy_command = proxy and { '/bin/sh', '-c', proxy } or { '/usr/bin/nc', '-U', os.getenv('FT_BG_SOCK') },
  skip_permissions_check = true,
} }

local ticker = { '/bin/sh', '-c', "i=0; while true; do i=$((i+1)); printf '\\033]0;tick-%d\\007' $i; sleep 1; done" }

-- A marker file, not a Lua variable: the config can be evaluated again.
local function setup_started()
  local f = io.open(H .. '/setup.started', 'r')
  if f then
    f:close()
    return true
  end
  f = io.open(H .. '/setup.started', 'w')
  f:write('1')
  f:close()
  return false
end

local function setup_done()
  local f = io.open(H .. '/setup.done', 'r')
  if f then
    f:close()
    return true
  end
  return false
end

ft.on('update-status', function(window, pane)
  if pane:get_domain_name() ~= 'e2e' then
    return
  end
  local mux_window = window:mux_window()
  if (ROLE == 'setup' or ROLE == 'both') and not setup_started() then
    mux_window:spawn_tab { domain = { DomainName = 'e2e' }, args = ticker }
    mux_window:spawn_tab { domain = { DomainName = 'e2e' }, args = ticker }
    mux_window:tabs()[1]:activate()
    local f = io.open(H .. '/setup.done', 'w')
    f:write(tostring(#mux_window:tabs()))
    f:close()
    return
  end
  if ROLE == 'setup' or not setup_done() and ROLE == 'both' then
    return
  end
  local titles = {}
  for _, tab in ipairs(mux_window:tabs()) do
    table.insert(titles, tab:active_pane():get_title())
  end
  local f = io.open(H .. '/titles.log', 'a')
  f:write(os.time() .. ' ' .. table.concat(titles, '|') .. '\n')
  f:close()
end)

return config
