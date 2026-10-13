# libssh-rs-sys

## FrankenTerm import provenance

Imported from the published `libssh-rs-sys` 0.2.8 crate archive, SHA-256
`568b2319d54c0d9094ba4bd1701eae2aedfa3739e18019f0d5c7dc4dc20dc033`.
The archive's `.cargo_vcs_info.json` identifies upstream
`https://github.com/wez/libssh-rs`, commit
`5cd2872a60a20cc84cea2c37a1388d2ef12b4653`, subdirectory `libssh-rs-sys`.
This is a separate dependency import, not part of the original WezTerm import.

Local changes retain OpenSSL and zlib native linkage through their Rust sys
dependencies, remove duplicate library-mode declarations from the build script,
and enable static zlib with `vendored`. The non-vendored source-build fallback
retains its system zlib link. Both Cargo manifests record the feature change.
The workspace uses a relative Cargo patch; the imported source travels in the
committed DSR source archive.

Bundled C sources (libssh 0.11.4) carry one security backport: upstream libssh
commit `44b186fa17aff497dae420c59c003222e438103c` ("channels: Fail when
receiving max packet size 0", CVE-2026-59843), applied by hand to
`vendored/src/channels.c` and `vendored/src/messages.c` because the context
differs slightly from upstream; the diff is the same 22 insertions and 4
deletions. A peer that advertises a maximum packet size of 0 in
SSH2_MSG_CHANNEL_OPEN or SSH2_MSG_CHANNEL_OPEN_CONFIRMATION is now refused
instead of leaving channel writes in an endless loop. libssh 0.11.5 and 0.12.2
ship the same fix. `build.rs` declares `rerun-if-changed` for itself and
`vendored/`; before, its `rerun-if-env-changed` lines alone stopped Cargo from
recompiling the C sources after they were edited.

FrankenTerm release and validation receipts come from DSR and RCH, respectively.
Upstream workflow badges are not qualification evidence for this import.

Native bindings to [libssh](https://www.libssh.org/).

## Features

The `vendored` feature causes a static version of libssh to be compiled and linked into your program.
If no system `libssh` is detected at build time, or that system library is too old, then the vendored
`libssh` implementation will be used automatically. Note that the `libssh-rs` bindings make use of
a couple of new interfaces that have not made it into a released version of `libssh` at the time
of writing this note, so all users will be effectively running with `vendored` enabled until libssh
releases version `0.9.7`.

The `vendored-openssl` feature causes a vendored copy of `openssl` to be compiled and linked into your program.

On macOS and Windows systems, you most likely want to enable both `vendored` and `vendored-openssl`.

## License

This crate is licensed under the MIT license, and is:
Copyright (c) 2021-Present Wez Furlong.

The bundled C library has separate licensing from the Rust binding package.
Its source and notices are retained in `vendored/`, including `COPYING`, `BSD`,
and per-file copyright/license headers. This directory contains the published
archive's actual C sources, not a Git submodule. The Rust package's MIT
declaration does not replace those bundled licenses.
