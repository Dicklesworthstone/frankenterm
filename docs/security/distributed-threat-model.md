# Distributed Wire Protocol Threat Model

**Bead:** [BR-RC-SAFETY-PROOFS.G11] / `ft-x0666.3`
**Production code:** `crates/frankenterm-core/src/distributed.rs`
(6.1k LOC), `crates/frankenterm-core/src/wire_protocol.rs`
(2.5k LOC)
**Status:** First-cut threat model. Mitigations-in-place are
shipped. Mitigations-pending are tracked as follow-on beads.

## Scope

This document covers the wire-level protocol between
`wa-agent` instances and the aggregator. It does NOT cover the
full distributed system (consensus, leader election, log
replication) — those are explicit non-goals (§ Non-Goals).

## Assets

| # | Asset | Confidentiality | Integrity | Availability |
|---|---|---|---|---|
| A1 | Pane capture stream (raw bytes from agents' panes) | High | High | Medium |
| A2 | Pattern detection events (rule_id + extracted JSON) | Medium | High | Medium |
| A3 | Sender identity (which agent emitted what) | Medium | High | Medium |
| A4 | Aggregator's per-sender dedup state | Low | High | High |
| A5 | Audit ledger (forensic trail of accepted messages) | High | High | High |
| A6 | Storage backend (sqlite + tantivy) | High | High | Medium |

## Trust boundaries

```text
   ┌──────────────┐         ┌──────────────┐         ┌──────────────┐
   │   Agent      │   →     │  Aggregator  │   →     │    Storage   │
   │ (wa-agent)   │  wire   │ (Aggregator) │ trusted │  (sqlite/    │
   │              │         │              │         │   tantivy)   │
   └──────────────┘         └──────────────┘         └──────────────┘
        │                          │                        │
        │   ← UNTRUSTED →          │   ← TRUSTED  →         │
        │                          │                        │
        └─ adversary may sit ──────┘
           anywhere in the network
```

The **wire** is the trust boundary. The aggregator MUST treat
every byte from the wire as adversarial input.

## Adversaries

### T1 — Network attacker (passive)

Reads wire traffic. Goals: extract pane contents, learn agent
identities, build behavior profiles.

### T2 — Network attacker (active)

Modifies, drops, replays, reorders, duplicates wire frames.
Goals: corrupt aggregator state, force replay of old commands,
poison detection events with attacker-chosen content.

### T3 — Compromised aggregator

Out of scope for this bead's headline rule (the aggregator IS
the trust anchor). Tracked as a separate concern; mitigations
favor minimizing aggregator privilege.

### T4 — Compromised agent

A single agent emitting malformed / oversized / impersonated
messages. Goals: poison cross-agent dedup, attribute events to
a different agent, exhaust aggregator resources.

### T5 — Replay adversary (offline)

Captures legitimate traffic, replays it later. Goals: force the
aggregator to re-run side effects (storage writes, downstream
event dispatch) for a stale envelope.

## Mitigations-in-place (verified)

| ID | Threat | Mitigation | Verified by |
|---|---|---|---|
| M1 | T2 reorder | Per-sender monotonic seq frontier; lower-or-equal seqs become Duplicate | `wire_dedup_model` BFS proof — convergence under all permutations |
| M2 | T2 duplicate | Same as M1 | Same |
| M3 | T2 drop | Aggregator does not require contiguous seqs; gaps are reported via separate `GapNotice` channel | `wire_dedup_model` `drop_subset_yields_equal_or_lower_frontier` |
| M4 | T5 replay | Per-sender frontier — once seq N accepted, all (sender, seq ≤ N) are duplicates forever within the session | `wire_dedup_model` `replay_attempt_never_accepts` + `lower_seq_after_high_is_always_duplicate` |
| M5 | T4 oversized payload | `MAX_MESSAGE_SIZE` cap (1 MiB default; tunable) checked **before** JSON parse | `wire_protocol.rs:from_json_with_limits` + `wire_envelope` fuzz target |
| M6 | T4 oversized identity | `MAX_SENDER_ID_LEN` cap on the `sender` field | `validate_envelope_protocol_with_limits` |
| M7 | T2 wrong-version envelope | `PROTOCOL_VERSION` field gates accept; `VersionMismatch` rejected | `wire_envelope` fuzz target |
| M8 | T4 stale-session resource exhaustion | `prune_stale_agents` evicts sessions idle > `stale_after_ms`; capacity-based eviction when `max_agents` reached | `wire_protocol.rs:prune_stale_agents` |
| M9 | T1 + T2 (production guidance) | Loopback default; non-loopback bind without TLS triggers `ft doctor` warning | Tracked: bead action #5 (CI assert) |
| M10 | T2 panic / DoS via crafted JSON | Differential JSON fuzz at `wire_envelope` and `ipc_auth_envelope` targets | cargo-fuzz lanes |

## Mitigations-pending (follow-on beads)

| ID | Threat | Mitigation | Tracking |
|---|---|---|---|
| P1 | T4 origin-spoofing | Ed25519 per-agent identity; signature over (envelope sans signature) | Bead action #4 (round-3 addition) |
| P2 | T2/T4 differential parsing across versions | Cross-version diff fuzz (vN encode → vM decode) | Bead action #2 (depends on a v2 envelope existing; currently `PROTOCOL_VERSION = 1`) |
| P3 | T1 wire confidentiality | TLS on non-loopback binds (mutual TLS for agent ↔ aggregator) | Operator-deployment concern; `ft doctor` warning is the in-band signal |
| P4 | T3 / cross-host audit replication | Reed-Solomon erasure encoding spec for the audit ledger | Bead action #6 (alien-artifact uplift; optional) |
| P5 | T1 sender identity confidentiality | Pseudonymous sender ids; rotate per session | Out of scope — defer to deployment policy |

## Non-goals

- **Full Byzantine consensus.** The aggregator is the trust
  anchor; cross-aggregator consensus is not in this protocol's
  scope. Multi-aggregator deployment is a future architectural
  decision.
- **End-to-end encryption to the storage layer.** Pane capture
  bytes are stored cleartext (or with field-level redaction
  per the redactor); transport encryption (TLS) is the wire
  confidentiality layer.
- **Strong real-time guarantees.** The protocol is best-effort
  with eventual consistency under M1–M3.

## Cross-references

- **Production code:** `crates/frankenterm-core/src/wire_protocol.rs`
  (envelope, aggregator, dedup) and
  `crates/frankenterm-core/src/distributed.rs` (transport,
  routing, lifecycle).
- **Dedup proof:** `crates/frankenterm-core/src/wire_dedup_model.rs`
  + `tests/wire_dedup_model.rs` + `docs/specs/wire-dedup.tla`.
- **Audit-doc cross-link:** `docs/security/wire-protocol-attestation.md`
  (this bead's audit doc).
- **Sibling threat models:**
  `docs/security/policy-denial-audit-wiring-matrix.md`,
  `docs/security/policy-rate-limit-asymmetry.md`,
  `docs/security/read-path-redaction-matrix.md`,
  `docs/security/passive-watch-attestation.md`.
- **Attestation graph:** `BR-RC-FOUNDATION.G3.1` /
  `ft-syqcz.1` — once the schema lands, the per-release
  attestation entry for the dedup proof is authored.
