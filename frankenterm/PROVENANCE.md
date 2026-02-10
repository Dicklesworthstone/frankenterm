# FrankenTerm Provenance

Source: https://github.com/wez/wezterm
Commit: 05343b387085842b434d267f91b6b0ec157e4331
Date imported: 2026-02-10
License: MIT (see upstream LICENSE.md)

## Scope

29 crates from the WezTerm workspace (transitive closure of codec, config,
mux, wezterm-term). Does NOT include GUI, font, window system, or server
binary crates.

## Crates included

async_ossl, bintree, codec, config, wezterm-config-derive, filedescriptor,
luahelper, mux, portable-pty, procinfo, promise, rangeset, termwiz,
termwiz-funcs, umask, vtparse, wezterm-bidi, wezterm-blob-leases,
wezterm-cell, wezterm-char-props, wezterm-color-types, wezterm-dynamic,
wezterm-dynamic-derive, wezterm-escape-parser, wezterm-input-types,
wezterm-ssh, wezterm-surface, wezterm-term, wezterm-uds

## Ownership

This code is now owned by the wezterm_automata project. Radical modifications
(memory leak fixes, Lua removal, custom allocators, event hooks) are expected.
Upstream compatibility is NOT maintained.
