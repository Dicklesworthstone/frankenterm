# Static Attestation Helper Contract

`ft-gv7y2.1` adds shared helper APIs for repo-local static attestation
verifiers. These helpers are for scripts that validate committed documents,
fixtures, manifests, source references, and proof metadata without running
Cargo or claiming runtime proof.

## Helper Files

- `tests/scripts/static_attestation_helpers.rb` exposes the Ruby API used by
  JSON-heavy verifiers.
- `tests/scripts/static_attestation_helpers.sh` exposes sourceable Bash helpers
  for shell wrappers.
- `tests/scripts/test_static_attestation_helpers.rb` is the regression self-test
  for the helper contract.

The helpers live under `tests/scripts/` so adding the sourceable shell helper
does not change the stamped `tests/e2e/**/*.sh` count.

## Ruby API

Ruby verifiers should load the helper with:

```ruby
$LOAD_PATH.unshift File.expand_path("../scripts", __dir__)
require "static_attestation_helpers"
```

Primary entry points:

- `StaticAttestation.repo_relative_path!` rejects empty, absolute, NUL-bearing,
  empty-segment, dot-segment, and parent-traversal paths.
- `StaticAttestation.require_file!`, `read_text!`, and `read_json!` validate and
  read repo-relative files.
- `StaticAttestation.expected_strings` preserves each expected string as a whole
  phrase. It deliberately does not split multi-word expectations.
- `StaticAttestation.require_terms!` and `require_file_terms!` assert exact
  phrase presence.
- `StaticAttestation.require_source_documents!` checks source-document lists.
- `StaticAttestation.require_seed_corpus!` compares declared seed names and byte
  sizes against the corpus directory.
- `StaticAttestation.require_direct_exec_script!` verifies a script has a
  shebang, executable bit, and `set -euo pipefail`.
- `StaticAttestation.expect_failure!` records negative static fixtures that must
  fail helper validation, such as absolute paths, raw-content privacy flags, or
  local-Cargo proof claims.

Each helper emits one JSONL record per check to stderr unless
`STATIC_ATTESTATION_LOGS=0` is set. Log rows include `check`, `input_path`,
`expected`, `actual`, `status`, and `failure_reason` when present. String values
that look unsafe for logs are replaced with SHA-256 fingerprints and byte
counts, so helpers are suitable for manifests and paths but not for dumping pane
content.

## Shell API

Shell verifiers can source:

```bash
source "tests/scripts/static_attestation_helpers.sh"
```

Available functions:

- `static_attestation_require_command`
- `static_attestation_require_repo_relative_path`
- `static_attestation_require_file`
- `static_attestation_require_executable_script`
- `static_attestation_run_ruby`

`static_attestation_run_ruby` sets the Ruby load path so inline Ruby checks can
use `StaticAttestation` without duplicating helper bootstrapping.

## Adopted Verifiers

| Verifier | Helper coverage | Proof boundary |
|---|---|---|
| `tests/e2e/test_passive_watch_attestation_manifest.sh` | Direct-exec shape, source-document existence, seed corpus names and byte sizes, exact audit/source terms. | Static artifact consistency only; the targeted unit proof remains RCH-gated by the attestation status. |
| `tests/e2e/test_adversarial_contract_fuzz_manifest.sh` | Direct-exec shape, repo-relative target/schema/corpus paths, JSON schema parsing, workflow matrix parity, source-term checks, negative privacy/path/local-proof fixtures. | Static manifest and CI-contract consistency only; Cargo/fuzz compilation remains the separate `rch exec` proof named in `local_proof.commands`. |

## Proof Boundary

These checks are static proof only:

- JSON parseability and schema/manifest field consistency.
- Repo-relative path plus empty-segment, dot-segment, and parent-traversal
  rejection.
- Source document existence.
- Seed corpus file name and byte-size consistency.
- Direct-exec, shebang, and strict-mode script shape.
- Multi-word expected phrase preservation.
- Negative fixture behavior for malformed static contracts.

These checks require downstream RCH-backed proof before a bead can claim runtime
behavior:

- Any Cargo build, unit test, integration test, cargo-fuzz target, or clippy
  result.
- Claims that a feature works in the live CLI, Robot Mode, MCP, daemon, GUI, or
  mux runtime.
- Claims that an attestation proves parser, redaction, storage, or policy
  behavior beyond the committed fixture/source contract it statically checks.

Downstream beads should keep the distinction explicit in their closeout notes:
static helper proof can verify that artifacts and contracts are internally
consistent, while compiled/runtime proof remains RCH-only.
