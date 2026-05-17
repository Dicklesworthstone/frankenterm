# Robot SDK Transport Contract

**Bead:** `ft-gzgfc.1`
**Audience:** Agents implementing generated Robot Mode SDK clients.

The generated Rust, Python, TypeScript, and Go SDKs are finish-line supported
SDK targets today. Rust uses the in-process `RustSdkTransport`; Python,
TypeScript, and Go use tested default process transports that run
`ft robot --format json` without shell interpolation. A language may move from
template-only to supported only after it satisfies this contract and its
generated artifact no longer contains a placeholder transport.

This document defines the shared transport behavior that SDK targets must
implement before changing `SdkLanguage::is_fully_supported`.

## Current Support Matrix

| Language | Current state | Production artifact | Runtime constraint |
| --- | --- | --- | --- |
| Rust | `supported_daemon_transport` | `frankenterm_client_rust.rs` | Uses `RustSdkTransport` over watcher IPC. |
| Python | `supported_process_transport` | `frankenterm_client_python.py` | Uses `asyncio.create_subprocess_exec` to run `ft robot --format json` without shell interpolation. |
| TypeScript | `supported_process_transport` | `frankenterm_client_typescript.ts` | Node-only process transport using `node:child_process`; browser use requires a separate future transport. |
| Go | `supported_process_transport` | `frankenterm_client_go.go` | Uses `os/exec.CommandContext` with `context.Context` cancellation and timeouts to run `ft robot --format json` without shell interpolation. |

The checked fixture
`crates/frankenterm-core/tests/fixtures/robot_sdk_supported_matrix.json`
pins this table to `SdkLanguage::is_fully_supported()`,
`standard_contract_artifacts()`, and the generated source markers for every
language.

## Support States

Each SDK language has exactly one support state:

| State | Meaning | Artifact rule |
| --- | --- | --- |
| `template_only` | The generated source is an example skeleton. The consumer must provide transport wiring. | The source must contain an explicit `transport not wired` marker, and docs must not advertise the language as supported. |
| `supported_process_transport` | The generated source has a tested default transport that shells out to `ft robot`. | The source must not contain a placeholder marker, and tests must cover success and failure envelopes. |
| `supported_daemon_transport` | The generated source has a tested default transport that talks to a future daemon IPC/API. | The source must not contain a placeholder marker, and tests must cover daemon unavailable and protocol failure. |
| `supported_pluggable_transport` | The generated source ships a tested interface and a supported default implementation. | The default implementation must be real; an interface alone is still `template_only`. |

Promotion rule: a language can return `true` from
`SdkLanguage::is_fully_supported()` only when its generated source is in one of
the `supported_*` states and the same change updates tests, artifact guards,
and documentation.

## Transport Shape

New supported SDK transports should prefer the process transport unless a
daemon IPC surface is already available and proven. Process transport is less
elegant, but it keeps the SDK contract aligned with the public CLI and avoids
inventing a parallel protocol.

A process transport call is:

```text
ft robot --format json <command> <args...>
```

The transport MUST:

- locate the `ft` binary from explicit configuration first, then `PATH`;
- pass Robot Mode arguments without shell interpolation;
- request JSON output unless the language has a documented TOON parser;
- parse stdout as a `RobotResponse` envelope;
- treat nonzero exit, invalid JSON, timeout, and missing binary as transport
  errors, not robot business errors;
- preserve stderr as bounded diagnostic text and avoid logging pane content;
- expose a configurable timeout per call;
- keep an injectable transport seam for tests and advanced callers.

The transport MUST NOT:

- call local heavy Cargo or build FrankenTerm as part of a normal SDK call;
- parse human CLI output;
- silently convert transport failures into successful empty responses;
- widen supported command coverage beyond what the generated method knows how
  to encode and decode.

## Envelope Handling

All supported SDK transports decode the same Robot Mode envelope:

```json
{
  "ok": true,
  "data": {},
  "elapsed_ms": 1
}
```

For `ok: true`, return the decoded `data` payload in the language-native shape
generated for that method.

For `ok: false`, raise or return a robot error that preserves:

- `error_code`;
- `message`;
- `hint`, if present;
- `details`, if present;
- `elapsed_ms`, if present.

Robot errors are not transport errors. A policy denial, unsupported command, or
capability-unavailable response is a successful transport exchange with a
negative robot result.

## Required Fixture Cases

Every promoted language needs fixture-backed tests for these cases:

| Case | Input | Expected result |
| --- | --- | --- |
| success envelope | stdout contains `{"ok":true,"data":...}` and exit status is 0 | method returns decoded data |
| robot error envelope | stdout contains `{"ok":false,"error_code":"robot.policy_denied",...}` and exit status is 0 | method reports a robot error with code and hint preserved |
| invalid payload | caller omits or mis-types a required method argument | SDK rejects before invoking `ft` when possible |
| unsupported command | generated method asks for a command outside the transport allow-list | SDK reports unsupported command without invoking `ft` |
| timeout | process does not complete before deadline | SDK reports transport timeout with bounded diagnostics |
| missing binary | configured or PATH binary is absent | SDK reports transport unavailable |
| invalid JSON | process exits 0 but stdout is malformed | SDK reports protocol decode failure |
| stderr with success | process exits 0 and stdout is valid, stderr has warnings | method succeeds and preserves bounded diagnostics when exposed |
| nonzero exit | process exits nonzero with or without JSON | SDK reports transport failure unless a valid robot envelope is explicitly documented for that path |

Language-specific tests may use fake process runners instead of spawning a real
binary. At least one repository proof lane must validate the generated source
text and artifact bundle with RCH-backed Rust tests.

## Language Requirements

### Python

The Python client exposes an async-friendly API backed by
`asyncio.create_subprocess_exec`. It provides an injectable callable for tests,
validates known command payloads before spawning `ft`, and reports robot errors
separately from transport errors.

The generated source must not contain
`NotImplementedError("transport not wired")`.

### TypeScript

The TypeScript client ships a Node-only default process transport using
`node:child_process`. Browser support requires a separate daemon or fetch
transport and must not be implied by the child-process implementation. The
generated client exposes a `ProcessRunner` interface that tests and advanced
callers can satisfy without spawning a process.

The generated source must not contain
`throw new Error("transport not wired...")`.

### Go

The Go client uses `context.Context` for cancellation and timeout propagation
and exposes an injectable `ProcessRunner` for fixture tests. Robot errors and
transport errors are distinguishable, and generated code must be deterministic
and `gofmt` compatible.

The generated source must not contain `panic("transport not wired")` or any
other placeholder transport default.

## Artifact Guards

When a language remains `template_only`, tests must assert that the marker is
still present and the language is not included in the production artifact
bundle as supported.

When a language is promoted, tests must assert all of the following:

- generated source for that language does not contain `transport not wired`;
- `SdkLanguage::<Lang>.is_fully_supported()` returns true;
- the contract artifact bundle includes the promoted source;
- the promoted source includes a real default transport or a real default
  transport factory;
- SDK docs list the language as supported and identify any runtime constraint,
  such as TypeScript's Node-only process transport or Go's
  `context.Context`-driven process transport.

The artifact bundle must never mix a `supported_*` language state with a
placeholder transport marker.

## Proof Requirements

Implementation closeout for each language must include:

- RCH-backed Rust tests for deterministic code generation and artifact bundle
  guards;
- language-level fixture tests for envelope and transport behavior;
- `git diff --check` on touched files;
- formatter checks for generated source where practical (`gofmt`, TypeScript
  formatter or compiler, Python syntax check);
- documentation updates that state remaining unsupported languages honestly.

If a language-specific toolchain is unavailable, record that as an environment
blocker and keep the language in `template_only`.
