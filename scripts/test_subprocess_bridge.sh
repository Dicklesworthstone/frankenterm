#!/usr/bin/env bash
set -euo pipefail

rch exec -- cargo test -p frankenterm-core --features subprocess-bridge --lib subprocess_bridge -- --nocapture
echo "PASS: SubprocessBridge tests complete"
