# Wire Dedup Spec Mapping

Spec: `wire-dedup.tla`

## Rust Correspondence

| TLA+ symbol | Rust target | Notes |
|---|---|---|
| `DedupSession` | `crates/frankenterm-core/src/wire_dedup_model.rs:77` | Per-sender frontier and duplicate counters. |
| `DedupModelState` | `crates/frankenterm-core/src/wire_dedup_model.rs:111` | Map of sender ids to dedup sessions. |
| `Aggregator` | `crates/frankenterm-core/src/wire_protocol.rs:540` | Production distributed wire-protocol aggregator. |

## Action Mapping

| TLA+ action | Rust target | Notes |
|---|---|---|
| `Ingest` | `crates/frankenterm-core/src/wire_dedup_model.rs:139` | Model transition for accepting or skipping an envelope. |
| Production ingest branch | `crates/frankenterm-core/src/wire_protocol.rs:594` | Runtime envelope ingestion entrypoint. |
| Duplicate branch | `crates/frankenterm-core/src/wire_protocol.rs:639` | `messages_received > 0 && seq <= last_seq` skip rule. |
| Accept branch | `crates/frankenterm-core/src/wire_protocol.rs:649` | Frontier and accepted-message counter update. |

## Invariant Mapping

| TLA+ invariant | Rust target | Notes |
|---|---|---|
| `MonotonicFrontier` | `crates/frankenterm-core/src/wire_dedup_model.rs:214` | Last accepted sequence never falls behind observed history. |
| `NoReplay` | `crates/frankenterm-core/src/wire_dedup_model.rs:214` | Duplicate seqs are not accepted as new messages. |
| `TotalEventsBalance` | `crates/frankenterm-core/src/wire_dedup_model.rs:214` | Accepted plus duplicate counts match delivered history. |
| `SenderFrontierMatchesHighSeqAccepted` | `crates/frankenterm-core/src/wire_dedup_model.rs:214` | Session frontier corresponds to an accepted event. |

## TLC Configuration

Config: `wire-dedup.cfg`

The deterministic smoke model uses two senders and `MaxSeq = 2`, enough to cover
first-message-at-zero, duplicate replay, and per-sender independence.
