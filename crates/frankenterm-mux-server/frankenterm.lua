-- FrankenTerm mux server config (remote-side).
--
-- Deployed to each remote host's ~/.config/frankenterm/frankenterm.lua (the
-- systemd user service runs `frankenterm-mux-server --config-file=…` against
-- this exact path, bypassing the default config-discovery search so the
-- mux never accidentally inherits a sibling wezterm config that lives at
-- ~/.wezterm.lua or ~/.config/wezterm/wezterm.lua).
--
-- Sizing/tuning matches the agent-swarm profile the host already uses for
-- wezterm-mux-server. Everything GUI-shaped is omitted — fonts, colors,
-- keybindings, ssh_domains have no effect on the headless mux.

-- The `frankenterm` module aliases the same backing table as `wezterm`, so
-- either require name works. We prefer the project-native name for clarity;
-- the alias still resolves on older mux binaries that only register the
-- legacy `wezterm` module name.
local frankenterm = require 'frankenterm'
local config = frankenterm.config_builder()

-- Login shells so .bashrc / .zshrc fire and prompts/PATH look right.
config.default_prog = { '/bin/bash', '-l' }

-- The mux server creates and binds its socket at
-- $XDG_RUNTIME_DIR/frankenterm/sock automatically (UnixDomain default
-- behavior; see frankenterm/config/src/unix.rs::UnixDomain::socket_path
-- and frankenterm/config/src/config.rs::compute_runtime_dir).
-- On Ubuntu the path resolves to /run/user/<uid>/frankenterm/sock, which
-- is distinct from the wezterm-mux-server socket at
-- /run/user/<uid>/wezterm/sock.
--
-- We don't override `config.unix_domains` here; the default
-- `UnixDomain::default_unix_domains()` returns a single entry named "unix"
-- which is what the mux server uses to bind.

-- AI-swarm tuning: large scrollback, big parser buffer, snappier coalesce
-- delay. Mirrors the local wezterm.lua so output behavior is consistent
-- across the local and remote muxes.
config.scrollback_lines = 100000
config.mux_output_parser_buffer_size = 512 * 1024
config.mux_output_parser_coalesce_delay_ms = 3

return config
