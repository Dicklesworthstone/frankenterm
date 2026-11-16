# ft-7h5da.12.6 Killed Ideas Register

Status: tombstone artifact for `ft-7h5da.12.6`.

This register records duel ideas that should not be resurrected without new
evidence. Each entry includes the specific failure mode and the only salvage
kernel, if one exists. Treat this file as a negative design decision: a future
bead may reopen an entry only by citing new facts that invalidate the reason
below.

## Reopen Rule

Do not reopen an entry because the name sounds useful. A reopening bead must
include:

- the new evidence;
- the old failure mode from this register;
- the narrowed scope that avoids that failure;
- a proof plan that checks the avoided failure does not return.

## Tombstones

| Idea | Why it was killed | Salvage, if any |
| --- | --- | --- |
| CRDT active-active mission state | One-shot approval tokens, mutual-exclusion reservations, and hash-chained audit ledgers require linear authority. Eventual consistency would reverse a documented signed design decision and make approval consumption ambiguous. | None in the original scope. Keep mission state single-authority unless a later design proves token consumption and audit ordering remain linear. |
| Zero-trust mission marketplace | It inverts the local-authority doctrine of the wire protocol. Mid-mission partition breaks Tx atomicity, and the retry story double-executes committed work. | None in the original scope. Marketplace-like sharing must stay outside live mission execution unless Tx atomicity is preserved end to end. |
| Multimodal Visual-AST rendering | Rendering agent-observed output as images breaks capture fidelity, full-text search, and redaction, because images are not text. It also assumes agent CLIs consume visual context from scrollback. | An explicit `ft viz <file>` for humans only, never an output interception path and never a substitute for redacted text capture. |
| PTY RAG injection | It violates the passive-first observe/act split. Agents do not reliably read scrollback, and injecting retrieved text into the PTY creates a prompt-injection channel. | The safe kernel already exists as HandleOnErrorCassSearch: retrieve context as a typed error-handling aid, not as unsolicited pane input. |
| Operator first-run guided tour | It would become a fourth overlapping onboarding surface. The real fix is consolidation, not another tour. | Consolidate `ft demo` and `ft doctor` into one coherent onboarding/check surface when that work is prioritized. |
| Agent Mail outage spool | A local authoritative spool can split-brain around async mutual exclusion during an outage. The authoritative coordination surface is already being handled by existing Agent Mail failover work. | Keep only non-authoritative message intent records. Do not treat a local spool as accepted delivery or lock ownership. |

## Guardrails

- A killed idea can be cited as background, but not as acceptance criteria.
- A salvage kernel must be filed as a new bead with its own name and scope.
- If a future implementation needs to use the killed idea's original name, its
  design must explicitly explain why the old failure mode no longer applies.
