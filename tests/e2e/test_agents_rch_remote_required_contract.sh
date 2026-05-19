#!/usr/bin/env bash
# Static verifier for AGENTS.md remote-required RCH proof examples.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

AGENTS="AGENTS.md"

fail() {
  printf 'agents rch remote-required contract: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

require_file() {
  [[ -f "$1" ]] || fail "missing file: $1"
}

require_command ruby
require_file "${AGENTS}"

ruby <<'RUBY'
AGENTS = "AGENTS.md"

def fail!(message)
  warn "agents rch remote-required contract: #{message}"
  exit 1
end

text = File.read(AGENTS)

compiler = text[/## Compiler Checks \(CRITICAL\).*?---/m]
fail!("missing Compiler Checks section") unless compiler

manual = text[/## RCH — Remote Compilation Helper.*?### When rch is down/m]
fail!("missing RCH helper section") unless manual

required_snippets = [
  "RCH_REQUIRE_REMOTE=1",
  "RCH_NO_SELF_HEALING=1",
  "rch --no-self-healing exec --"
]

[["compiler", compiler], ["manual", manual]].each do |name, section|
  required_snippets.each do |snippet|
    fail!("#{name} section missing #{snippet}") unless section.include?(snippet)
  end
  fail!("#{name} section still has bare rch exec") if section.match?(/(^|\s)rch exec --\s+env\s+CARGO_TARGET_DIR=/)
end

%w[
  cargo\ check
  cargo\ clippy
  cargo\ fmt
].each do |command|
  fail!("compiler section missing #{command}") unless compiler.include?(command.tr("\\", ""))
end

%w[
  cargo\ build
  cargo\ test
  cargo\ clippy
].each do |command|
  fail!("manual section missing #{command}") unless manual.include?(command.tr("\\", ""))
end

fallback_needles = [
  "[RCH] local",
  "running locally",
  "no admissible workers",
  "worker=null",
  "local fallback",
  "blocked",
  "Do not count local Cargo output as proof"
]
fallback_needles.each do |needle|
  fail!("AGENTS.md missing fallback blocker wording: #{needle}") unless text.include?(needle)
end

puts "agents rch remote-required contract: static verifier passed"
RUBY
