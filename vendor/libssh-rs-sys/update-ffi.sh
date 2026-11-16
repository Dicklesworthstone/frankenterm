#!/bin/bash
# Regenerate the ffi bindings
cat >binding.h <<-EOT
typedef unsigned long size_t;
#include <libssh/libssh.h>
#include <libssh/callbacks.h>
#include <libssh/server.h>
#include <libssh/sftp.h>
#include <libssh/ssh2.h>
EOT

touch vendored/include/libssh/libssh_version.h

bindgen \
  binding.h \
  -o src/lib.rs \
  --no-layout-tests \
  --no-doc-comments \
  --raw-line "#![allow(non_snake_case)]" \
  --raw-line "#![allow(non_camel_case_types)]" \
  --raw-line "#![allow(non_upper_case_globals)]" \
  --raw-line "#![allow(clippy::unreadable_literal)]" \
  --raw-line "#![allow(clippy::upper_case_acronyms)]" \
  --default-enum-style rust \
  --constified-enum ssh_error_types_e \
  --constified-enum ssh_known_hosts_e \
  --constified-enum ssh_auth_e \
  --constified-enum ssh_keytypes_e \
  --allowlist-type '(sftp|ssh).*' \
  --allowlist-function '(sftp|ssh).*' \
  --allowlist-var 'SSH.*' \
  --verbose \
  -- \
  -Ivendored/include 

rm vendored/include/libssh/libssh_version.h
