# Handoff docs

Per-session handoff notes from the swarm. Each agent that runs a
substantive cycle drops a `<agent>-session-<date>.md` here so the
next agent can resume without re-deriving context.

## Index

- [cc_2-session-2026-05-02.md](./cc_2-session-2026-05-02.md) —
  19 deliverables across 14 beads (atlas substrate completion,
  Profile family handler, LabRuntime nightly CI lane, JSONL
  telemetry, doctor surfaces, RPC envelope types, regex catalog,
  storage callsite analyzer + recipe guide + 2 CI gates,
  storage_convert example, Row accessor substrate, blit budget
  calculator, operator playbooks, demo-full preflight verifier,
  per-PR diff helper). 7 closed + 8 with substrate progress.

## Encrypted Capsules

Production handoff capsule encryption should use
`XChaCha20Poly1305Hook` from
`crates/frankenterm-core/src/handoff_capsule_encryption.rs` or a
deployment-specific `CapsuleEncryptionHook`. The built-in hook requires a
32-byte symmetric key, or a 64-character hex string via `from_hex_key`.
Missing, wrong-length, and all-zero keys fail closed.

The sealed hook payload is `magic || key_id || nonce || ciphertext+tag`.
The outer `EncryptedCapsuleEnvelope` hashes that payload before decrypt,
so envelope tampering is rejected before the AEAD open path runs.

`XorPlaceholderHook` is compiled only for tests/docs and is not available
to production builds. It exists only to exercise failure ordering in unit
tests. For key rotation, keep the old 32-byte key available until existing
capsules expire or provide a custom hook with deployment-specific
multi-key routing; a key-id mismatch is reported without logging key
material or plaintext.

## Conventions

- File name: `<agent_id>-session-<YYYY-MM-DD>.md`. When an agent
  runs across midnight UTC, use the date the session started.
- Top of file: a one-line summary + closed/open bead counts so
  the index above can stay terse.
- Body: the substrate-shipped table (bead | slice | commit |
  deferred wired-pass) is load-bearing; every other section is
  optional but recommended.
- "What the next agent picks up" section: ordered by impact,
  pointing at the highest-leverage wired-pass slices.
- Disk-pressure notes: when the session navigated emergency
  cycles, document the strategy that kept the swarm alive
  (which dirs were safe to remove, which were not).
