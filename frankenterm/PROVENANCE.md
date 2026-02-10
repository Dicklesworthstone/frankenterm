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

async_ossl, bintree, codec, config, frankenterm-config-derive, filedescriptor,
luahelper, mux, portable-pty, procinfo, promise, rangeset, termwiz,
termwiz-funcs, umask, vtparse, frankenterm-bidi, frankenterm-blob-leases,
frankenterm-cell, frankenterm-char-props, frankenterm-color-types, frankenterm-dynamic,
frankenterm-dynamic-derive, frankenterm-escape-parser, frankenterm-input-types,
frankenterm-ssh, frankenterm-surface, frankenterm-term, frankenterm-uds

## Ownership

This code is now owned by the FrankenTerm project. Radical modifications
(memory leak fixes, Lua removal, custom allocators, event hooks) are expected.
Upstream compatibility is NOT maintained.
