# Session Persistence (Snapshots)

ft’s session persistence system captures terminal-backend mux evidence (current
bridge: WezTerm) into SQLite snapshots so you can:

- Inspect the bounded metadata needed to plan a manual reconstruction after an unclean shutdown
- Preserve evidence before an operator-managed restart
- Inspect session state and compare pane-ID membership over time

Snapshot capture/inspection ships. Restore and restart execution do not: their
non-dry CLI paths fail closed before process or mux effects. The executable
restorer is library/test substrate, not a production recovery surface. This
system is designed for **mux topology + pane metadata**, not full process
checkpointing.

## What a snapshot contains

At a high level, a snapshot stores:

- **Layout topology**: deterministic window/tab grouping plus a size-inferred
  per-tab pane tree that can fall back to a flat layout (a `TopologySnapshot`)
- **Per-pane state schema**: pane id, cwd, optional command, terminal size +
  alt-screen flag, optional agent metadata, and optional redacted environment
  (a `PaneStateSnapshot`). Field presence depends on what the capture bridge
  actually supplied; a schema field is not proof that foreground process or
  agent continuity was captured.
- **Dedup/consistency witness**: a versioned, framed SHA-256 `state_hash` so
  identical snapshots can be skipped and persisted projections can be checked

The current topology schema v1 sorts numeric tab IDs for deterministic output.
It does not yet preserve user tab order or an incarnation-scoped active-tab
identity. The migration contract is
`docs/proposals/ft-7xqz4-8-10-1-tab-order-authority-contract.md`.

What it does **not** (currently) guarantee:

- Restoring interactive in-process state (REPL variables, editor buffers, etc.)
- Restarting shells, agents, or other foreground processes automatically
- Restoring historical scrollback or terminal render state, regardless of capture quality
- Preserving mux-domain identity, durable app-reopen tab order, titles, exact
  cell geometry, stable active-tab identity, window/workspace placement, or
  full window appearance

## Quick start

### 1) Save a snapshot

```bash
ft snapshot save
```

JSON output:

```bash
ft snapshot save -f json
```

Example shape:

```json
{
  "ok": true,
  "session_id": "sess-…",
  "checkpoint_id": 123,
  "pane_count": 10,
  "pane_state_estimate_bytes": 123456,
  "persisted_text_bytes": 65432,
  "truncated_pane_count": 0,
  "projection_complete": true,
  "projection_completeness": "complete",
  "projection_completeness_scope": "persisted_pane_text_budget",
  "verification": "verified_v2",
  "trigger": "manual"
}
```

Triggers:

- `--trigger manual` (default)
- `--trigger event`
- `--trigger pre_restart`
- `--trigger pre_shutdown`
- `--trigger shutdown`
- `--trigger startup`

The one-shot CLI maps `event`, `pre_restart`, `pre_shutdown`, and `shutdown`
to ordinary `Manual` capture authority while retaining the requested label in
bounded checkpoint metadata. Those labels do not grant the watcher's sticky
terminal-capture reservation. `startup` maps to `SnapshotTrigger::Startup`;
the production periodic/intelligent scheduler also uses that trigger for its
first capture.

### 2) List snapshots

```bash
ft snapshot list --limit 10
```

JSON output:

```bash
ft snapshot list --limit 10 -f json
```

Example shape:

```json
{
  "ok": true,
  "count": 1,
  "limit": 10,
  "offset": 0,
  "has_more": false,
  "snapshots": [
    {
      "checkpoint_id": 123,
      "session_id": "sess-…",
      "checkpoint_at": 1730000000000,
      "checkpoint_type": "shutdown",
      "checkpoint_role": "snapshot",
      "pane_count": 10,
      "pane_state_estimate_bytes": 123456,
      "state_hash": "…",
      "label": "before maintenance",
      "verification": "not_computed",
      "projection_verification": "unchecked_projection",
      "projection_scope": "checkpoint_summary"
    }
  ],
  "verification": "not_computed",
  "projection_verification": "unchecked_projection",
  "projection_scope": "checkpoint_summary"
}
```

### 3) Inspect a snapshot

```bash
ft snapshot inspect 123
ft snapshot inspect 123 --pane 42
```

JSON output:

```bash
ft snapshot inspect 123 -f json
```

### 4) Diff two snapshots

```bash
ft snapshot diff 123 124
```

JSON output:

```bash
ft snapshot diff 123 124 -f json
```

This command compares pane-ID membership only. It does not compare topology,
working directories, commands, terminal state, agent metadata, or cell
content; structured output declares `comparison_scope: pane_membership_only`.

### 5) Delete a snapshot

```bash
ft snapshot delete 123
```

Use `--force` to skip confirmation:

```bash
ft snapshot delete 123 --force
```

Deletion is durable and can change exact clean-checkpoint authority and later
reconciliation decisions. Confirm the exact checkpoint identity and retain a
verified backup before bypassing the prompt.

## Restore behavior

### Unclean-session detection

`SessionRestorer` contains a fail-closed library path for detecting sessions
whose `shutdown_clean` flag is `0`, but `ft watch` does not currently call it
or offer an automatic restore prompt. Use `ft session doctor` and inspect the
candidate checkpoint. Recovery mutations remain manual; startup
notification/prompt integration and safe executable restore are tracked work.

### `ft snapshot restore`

`ft snapshot restore <id> --dry-run` resolves a bounded, metadata-only
checkpoint descriptor through a read-only connection and prints a descriptor
and status report. It does not decode topology, load the full checkpoint
projection, or constitute an execution preflight or success guarantee.

Every non-dry invocation fails closed on every platform before checkpoint
resolution, database mutation, subprocess launch, process discovery, or mux
operation. Robot checkpoint rollback has the same contract: metadata-only
dry-run planning, with non-dry execution unavailable.

The library/test layout substrate can exercise windows, tabs, splits, local
working directories, and an explicit per-tab active-pane identifier when one
is present. Current pane-list-derived schema v1 snapshots do not preserve user
tab order or stable active-tab identity, and they do not establish mux-domain
identity, window/workspace placement, titles, exact cell geometry, terminal
render state, historical scrollback, processes, or agents. Captured
`output_segments` are arbitrary stream fragments rather than an authoritative
terminal-state snapshot and are never sent through PTY input.

### Continuous cold scrollback durability

The mux server continuously persists rows that leave the hot terminal viewport
under a per-pane durable UUID. Each acknowledged row is synchronized before its
offset becomes visible, v2 row records bind the exact serialized payload with
SHA-256, and logical retention pruning is recorded in a synchronized sequence
journal so stable row numbers survive a crash even before byte compaction.
Compaction writes the retained suffix plus its base-sequence header to a sibling
file, synchronizes it, atomically publishes it, and then collapses the journal.
The identity manifest binds its own canonical fields with SHA-256 and records
the next sequence explicitly, including when retention leaves zero rows. Reopen
ignores torn content/journal tails, rejects complete malformed records,
validates every retained serialized row, and resumes at the next exact sequence.
If the sink cannot acknowledge a row, the terminal now keeps that row in its
in-memory deque, does not advance the stable-row offset, and does not count a
cold spill. This can temporarily exceed the configured hot-memory target, but
it prevents a storage error from being converted into silent scrollback loss.

The mmap store's optional SQLite fallback is pane-authoritative rather than a
best-effort dual-write mirror. A pane that opens successfully on mmap never
silently switches histories after a later write failure. If mmap cannot be
established, SQLite records a durable authority marker (including for an empty
or cleared pane), so restart selects the same history and sequence space.
Logical-prune sequence journals are scanned under a fixed byte ceiling and
force the existing crash-safe log compaction before approaching that ceiling,
while the append path refuses to cross it if compaction fails. Long-lived panes
therefore retain hot rows on pressure instead of making their own recovery
journal unreadable.

Operators can discover and export this format directly:

```bash
ft session list-durable --format json
ft session export-durable <32-hex-durable-pane-id> --format json
```

The list path validates each private pane directory and checksummed identity
manifest. The export path opens the content log and sequence journal read-only,
refuses symlinks and concurrent replacement, bounds physical bytes, encoded
records, decoded rows, and transcript bytes, and re-reads the manifest to fence
concurrent publication. It excludes and reports a final unterminated record,
decodes styled rows through the bounded codec, reapplies the current redactor,
and publishes a new mode-0600 transcript with file-and-directory synchronization.
It neither repairs nor deletes the source and never sends recovered output into
a PTY. The older `ft session recover` command remains specific to the separate
64-hex flat mmap orphan format; format identity is never guessed from content.

This continuous spill is not yet a complete pane image: it covers cold/evicted
rows, while the current hot viewport, terminal parser/render state, PTY handles,
and child process remain mux-owned. A guardian plus hot-state checkpoint is
therefore still required before a mux process replacement can claim live
process continuity.

### Durable PTY guardian contract

Lossless mux replacement requires a process outside the mux lifetime to own the
native PTY master and child handle. The guardian is that authority; it is not a
second mux backend. `LocalPane` becomes a proxy to the same FrankenTerm mux and
terminal implementation after the following contract is implemented and
proven:

1. The per-user service manager starts one guardian before the mux. Its socket
   directory is private, its authentication token is a private regular file,
   and neither mux discovery nor a guessed pane UUID grants control authority.
   A newly started mux sends the token-authenticated, payload-free `Hello`
   bootstrap with a nil guardian-incarnation marker and receives the current
   nonzero incarnation in the typed success payload before issuing census or
   pane requests; no unauthenticated side file is incarnation authority.
2. `Spawn` carries a unique request identity, bounded serialized
   `CommandBuilder`, initial `PtySize`, and durable pane UUID. Retrying the exact
   request returns the original pane; reusing the identity with different bytes
   is a terminal protocol error.
3. The guardian exclusively owns the PTY master, child waiter/signaller, writer,
   resize handle, and raw-output reader. Mux exit or disconnect releases only a
   control lease; it cannot close the PTY or signal the child. The transport
   layer tracks authenticated connections per exact mux incarnation and calls
   the state machine's idempotent disconnect-retirement transition only after
   the final connection is proven gone. A delayed notification for an old
   incarnation cannot retire successor leases.
4. Exactly one mux generation holds the mutation lease for a pane. `Claim`
   returns a monotonically increasing fencing generation, and every input,
   resize, signal, and close request carries that generation. A delayed request
   from a retired mux is rejected before effect.
5. Each accepted input has an idempotency identity and an explicit effect
   acknowledgement. An ambiguous disconnect is reconciled by querying that
   identity; blindly resending input is forbidden. The guardian retains the
   original identity of every live or retained pane spawn for the pane's full
   lifetime. Other request/effect receipts use a bounded FIFO replay window;
   eviction cannot repeat an old mutation because its retired sequence or
   generation is still fenced by the pane state. An input whose disposition is
   still `accepted_not_durable` is pinned outside that FIFO until its exact
   durable or terminal-rejection acknowledgement arrives. A reverse effect-to-
   request index updates only its retained aliases at acknowledgement time;
   reconciliation cost is independent of unrelated fleet receipt volume. A
   runtime acknowledgement carries the full authenticated pane, mux
   incarnation, generation, sequence, effect UUID, and payload-digest
   fingerprint rather than the effect UUID alone. After a resolved receipt
   rotates and its UUID is reused under a later fence, a delayed journal
   completion for the old fingerprint is therefore rejected without changing
   the new pending input. Once a resolved receipt rotates, a query below the
   pane's consumed sequence fence returns `disposition_unavailable`, never
   `not_seen`; receipt eviction therefore cannot falsely authorize a resend.
   `not_seen` is reserved for an identity whose sequence fence proves that the
   input has not been consumed. A
   pending effect admits at most 64 distinct request aliases, so one ambiguous
   input cannot consume the global receipt ledger; excess aliases fail before
   mutating any retained receipt. When an ordinary effect rotates out of the
   bounded window, every surviving request alias for that effect rotates with
   it as one identity unit; an alias can never outlive and disagree with its
   effect fingerprint.
6. Raw PTY output is appended to a synchronized, checksummed sequence log before
   acknowledgement to the mux. Exact raw bytes are mandatory-encrypted at rest;
   redaction would change terminal semantics, so no plaintext persistence mode is
   permitted. Bounded segments carry nonzero identities, globally contiguous
   output sequences, and an exact predecessor segment/sequence/terminal-digest
   chain so rollover can neither reuse an output identity nor hide a gap. A new
   segment cannot accept output until its file and parent directory entry are
   synchronized. Key authority lives in a pinned mode-0700 capability
   directory. Each random 32-byte key is an immutable mode-0600, no-follow,
   single-link regular file whose lowercase ID is the first eight bytes of its
   SHA-256 fingerprint. Append-only, checksummed activation records start at
   generation one and remain contiguous; rotation synchronizes the new key and
   its directory entry before publishing the next activation. Existing segment
   writers retain their original cipher until rollover, and historical keys are
   never overwritten or discarded while an activation can reference them. A
   torn unactivated key from an interrupted rotation is preserved but cannot
   supersede the prior activation or satisfy historical segment lookup. The
   pinned active key and full activation inventory are revalidated before new
   cipher use or rotation, so external key mutation and concurrent activation
   advancement fail closed. An exact retry after activation acknowledgement
   loss reopens and synchronizes the immutable record, then verifies its
   decoded identity, referenced key, and the full contiguous inventory before
   returning success. A missing, truncated, symlinked, hard-linked,
   permission-unsafe, wrong-ID, or corrupt referenced key fails closed. Key
   bytes are zeroized when their in-memory authority is dropped and never enter
   `Debug`, argv, environment variables, receipts, dumps, or log fields.
   The guardian retains a bounded replay window and terminal-state
   checkpoints that bind parser version, rows/columns, raw-output sequence, hot
   viewport, cold-scrollback base sequence, and a content digest.
   A checkpoint must either serialize the complete incremental parser state or
   prove that its raw-output sequence ends at a parser ground-state boundary;
   screen rows alone cannot represent a partial CSI, OSC, or DCS sequence.
   The ground-state proof uses
   `frankenterm_escape_parser::parser::Parser::is_recovery_ground`, not the
   lower-level VT transition state alone: semantic Sixel, short-DCS, termcap,
   and tmux-control parsers can retain state outside that table. The checkpoint
   must carry the exact `RECOVERY_CHECKPOINT_PARSER_ID` and the exact output
   record sequence processed into the snapshotted terminal model. Capture waits
   for those identities to coincide; it never combines an older ground offset
   with newer screen state, because replaying that suffix would duplicate
   already-applied terminal effects.
7. `Attach` returns a census entry plus the newest verified checkpoint and raw
   output strictly after its sequence. The mux reconstructs the terminal parser
   behind a replay gate whose writer discards parser-generated device replies and
   whose clipboard, download, notification, and device-control handlers are inert.
   Replayed bytes must never write into the surviving child or invoke host-facing
   callbacks. The mux verifies the resulting digest and parser boundary before it
   atomically installs the live guardian writer and approved handlers and publishes
   the pane into topology. A gap, unsupported parser state, or digest mismatch
   keeps the live writer unreachable and quarantines the pane for transcript
   recovery instead of presenting invented state.

#### Canonical terminal checkpoint v1 inventory

The terminal payload is a semantic model, not a dump of Rust struct memory.
Its canonical encoding uses fixed field order and sorted map keys so equivalent
state has one byte representation and one digest. Version 1 must cover all of
the following before it can be published:

| Class | Required state |
|---|---|
| Boundary | Format version, durable pane UUID, parser compatibility identity, immutable output segment UUID, synchronized output record sequence and digest, rows, columns, payload length, and payload digest. |
| Screens | Primary and alternate line/cell/attribute content, stable-row base, active-screen selector, physical geometry, saved cursor for each screen, keyboard encoding stacks, and wrap markers. |
| Performer | Active pen and cursor, pending wrap, insert/autowrap/origin/reverse/synchronized-output modes, horizontal and vertical margins, saved DEC modes, keypad/cursor-key/newline/bracketed-paste modes, mouse modes, charset selection, tab stops, and keyboard encoding. |
| Metadata | Window/icon title, progress, current directory, palette overrides, sixel color registers, user variables, terminal program/version, and sequence numbers. |
| Unicode and layout | Active Unicode width version, bounded custom width map, Unicode-version stack, bidi state, focus/lost-focus fences, pixel geometry, and DPI. |
| Graphics | Every referenced image and out-of-band Kitty placement/transmission state under explicit byte/count/frame caps, or an explicit unsupported-graphics rejection. Silently omitting graphics is forbidden. |

Configuration objects, PTY writers, clipboard/download/notification/device
handlers, caches, worker threads, telemetry counters, and scheduler state are
capabilities or derived state and are never deserialized from a checkpoint.
Restore constructs a new terminal behind a discard writer and inert handlers,
validates and installs only the semantic model, replays the authenticated raw
suffix, compares the resulting canonical semantic digest, and only then swaps
in live capabilities atomically.

The decoder applies an outer payload-byte ceiling before allocation, then
independent row, cell, stack, map, string, custom-width, image, placement, and
frame ceilings with checked arithmetic. Unknown versions, duplicate or
noncanonical map keys, invalid enum tags, impossible geometry/margins/cursors,
line sequence numbers newer than the terminal sequence, incomplete graphics,
trailing bytes, and any canonical re-encode mismatch are terminal corruption.
The last verified checkpoint and raw-log prefix remain untouched for recovery.
8. Guardian census is the post-crash process authority; the last committed mux
   topology manifest supplies window/tab/workspace placement. Reconciliation is
   by durable pane UUID, never by mux-local numeric pane ID or PID.
9. Guardian shutdown is a separate, explicit operator transaction. Stopping or
   upgrading the mux must not imply guardian shutdown, child termination, log
   truncation, or retention reclamation.

The standalone guardian must not reproduce the mux's current per-pane reader
and parser thread costs. One bounded readiness loop owns the listening socket,
authenticated client connections, and every native PTY master descriptor;
bounded connection buffers and a fixed spawn-worker pool feed that loop. Child
status is reaped through bounded event/poll integration, not one permanent
waiter thread per pane. The admission ceiling applies before a socket, spawn
job, pane record, output buffer, or descriptor is published, and backpressure
must stop reads rather than allocate an unbounded per-pane backlog. A platform
without a safe supported readiness/child-reaping implementation remains on the
explicit legacy mux-owned path; it must not silently claim guardian continuity.

#### Guardian protocol v1 freeze

The first implementation must use one length-delimited binary frame with a
fixed protocol version and a hard encoded-frame ceiling. Authentication is an
HMAC-SHA-256 over the exact versioned header and payload bytes using a random
32-byte token read from the private guardian token file; the token itself is
never serialized, logged, or accepted from a command-line argument. The server
verifies only the outer bounded frame length before locating the fixed-size MAC
trailer, verifies that MAC in constant time, and only then decodes authenticated
header fields or looks up a pane. Responses use the same rule: decode produces
an opaque authenticated-response capability, and the client must match its
operation, incarnations, request UUID, originating request-payload SHA-256, pane
UUID, and effect UUID to the exact originating request, including the echoed
lease generation and sequence, before consuming its payload. Resulting claim
generations and next-sequence values
belong in the authenticated response payload rather than mutable correlation
header fields. Success payloads use an operation-typed fixed binary schema;
fixed-width census rows carry an explicit exit-status presence flag so every
`i32` exit value remains representable without a sentinel; when that flag is
absent, its value bytes must use the one canonical zero encoding. Success and
rejection constructors are the only public response-envelope creation paths,
and the common encoder and decoder both revalidate operation scope, typed reply
shape, and echoed identity. After frame correlation, the client
also matches pane, effect, generation, and sequence identities inside the
decoded success payload before exposing it. Only the resulting
correlated-response capability exposes the payload. Peer credentials are an
additional local-transport fence, not a replacement for the token.

Authenticated non-success responses carry a fixed, content-free rejection
code rather than an error string. The code's frozen classification must agree
with the envelope status: transient capacity/input-durability gates are
`rejected`, while an exact request that can never become valid is `terminal`.
Malformed or status-inconsistent rejection payloads fail before the correlated
client exposes their code.

Every request header contains all of the following:

- protocol version and operation discriminant;
- guardian incarnation UUID and requesting mux incarnation UUID;
- request UUID plus SHA-256 of the exact operation payload;
- durable pane UUID when the operation is pane-scoped;
- lease generation and monotonically increasing per-lease mutation sequence;
- operation idempotency UUID for any request that can create an external
  effect.

`Hello` is the sole bootstrap exception to the nonzero guardian-incarnation
rule. Its authenticated request and correlated response header carry a
canonical nil marker, while its fixed 16-byte success payload carries the
current nonzero guardian incarnation. It has no pane, lease, effect, or payload
scope. A nonnil request marker, trailing payload bytes, malformed response, or
use of nil by any other operation fails closed.

`Spawn` is the only request that may omit a pane lease. It carries a bounded
serialized command, environment, working directory, initial PTY size, durable
pane UUID, and spawn idempotency UUID. Repeating the same request UUID and
payload digest returns the original result. Reusing either the request UUID or
spawn idempotency UUID with different bytes is a terminal conflict and never
spawns. The serialized command writes through the payload byte ceiling during
encoding rather than allocating first and checking later; decoding requires
the one canonical v1 serialization byte-for-byte, so ignored JSON fields or
alternate encodings cannot carry authenticated hidden data. Its debug surface
is content-free. Request headers, response headers, retained effect
fingerprints, input-effect queries, and durability-completion identities also
omit content-derived SHA-256 values from `Debug`; raw payload omission alone is
not sufficient because a low-entropy input digest is dictionary-testable.
Resize geometry and the supported terminate signal have fixed
operation-tagged payloads, while claim, attach, close, checkpoint, replay,
lease retirement, and `Hello` reject hidden trailing payload bytes.
`QueryInputEffect` has a fixed operation-tagged payload containing the original
input mutation sequence and payload SHA-256. The effect UUID alone is
insufficient because a resolved bounded-window receipt may rotate and that UUID
may later be reused under a different fence; an exact mismatch fails closed
rather than returning the newer input's disposition.
`Census` is read-only and paginated under a fixed entry and byte cap.
The first-page request carries a canonical nil new-snapshot marker; the
guardian allocates a nonzero incarnation-local snapshot UUID, binds it to the
requesting mux incarnation, freezes that sorted durable-pane view, and returns
that UUID, total count, and next cursor. Continuation pages must echo the
returned nonzero UUID. Cross-incarnation UUID use is an identity conflict. A
bounded eight-snapshot FIFO permits continuation-page retries while limiting
memory. A rotated or unknown nonzero snapshot can never recreate state and
fails closed rather than applying an ordinal cursor to a newer pane set and
silently skipping
or duplicating a concurrent spawn. Both the response producer and correlated
consumer enforce the exact originating request's entry and encoded-byte page
ceilings in addition to the global protocol cap. Each fixed-width row carries pane UUID,
state, generation, claiming mux and next sequence when live, pending input
identity, exit status, and quarantine reason.
`Claim` names the observed prior generation and returns exactly the next
generation; it cannot skip, wrap, or revive a terminal pane. The frozen base
`Attach` reply acknowledges only the exact pane, generation, and next mutation
sequence. Checkpoint and bounded raw-output replay transfer belong to the later
output-journal protocol tranche; the identity-only base reply must not be
described as continuity evidence.

`Input`, `Resize`, `Signal`, live-pane `Close`, `Checkpoint`, and
`RetireLease` require the exact current lease. Mutation operations consume the
next exact nonzero lease sequence. Read-only `Attach`, `Replay`, and
`QueryInputEffect` require header sequence zero and never advance the mutation stream;
the query payload separately carries the original input sequence and digest;
on a live claimed pane they require the exact claiming mux and generation.
After exit or terminal retention, `Replay` and `QueryInputEffect` instead use
the exact retained generation as recovery authority because no live mutation
lease remains. A retention-only `Close` after observed child exit is the other
terminal-state exception: it requires the exact retained generation and
sequence zero, cannot signal the already-exited child, and only seals the
retained record. The guardian rejects a stale generation, wrong live mux
incarnation, duplicate mutation sequence with different bytes, mutation
sequence gap, or exhausted sequence before performing an effect. Input
effect queries distinguish `not_seen`, `accepted_not_durable`,
`durable_effect`, `terminal_rejected`, and `disposition_unavailable`; only
`not_seen` permits a resend of the same idempotency UUID, while
`disposition_unavailable` requires operator/runtime reconciliation without
replay. A newly accepted input acknowledgement can contain only one of the
three applied-effect states, never either query-only state. Resize, signal,
close, and checkpoint operations
have the same request-digest replay rule, so an ambiguous response is queried
or replayed by identity rather than converted into a second effect.

The implementation exposes runtime effects only through a transactional API,
not the pure observation surface. It validates authentication, incarnation,
idempotency, capacity, pane state, generation, and sequence before invoking a
new-effect callback. An exact request or effect replay returns its retained
receipt without invoking that callback. The pane transition and new receipt
are committed only after callback success; receipt capacity is preflighted but
its exact eviction plan is likewise deferred until success. A zero-effect
spawn/resize/signal failure therefore cannot create a phantom pane, consume a
sequence, or discard an older replay receipt. Callback failure is
permitted only when no bytes, signal, resize, process, or other external effect
became observable. A possibly partial input write is therefore committed as
`accepted_not_durable` and reconciled by its exact effect UUID; it must never be
reported as a safely retryable callback failure.

The guardian input-effect journal makes that callback rule crash-safe without
persisting raw keystrokes. For each input it synchronizes the encrypted exact
identity and then a conservative `accepted_not_durable` marker before calling
any PTY write that could expose bytes. A definitely zero-byte result may refine
that marker to `known_not_applied`; a successful or possibly partial result may
only refine it to `durable`, or remain conservatively pending. The payload
SHA-256 is encrypted rather than written as plaintext because low-entropy key
events are dictionary-testable. The input log uses an input-domain-separated
AEAD surface of the activated guardian journal key; rotation must retain an old
key generation while either an output segment or input log still references
it. Each fixed-size record authenticates its clear
framing and predecessor digest, and recovery enforces monotonic journal order,
lease-generation input order, exact legal transitions, bounded effect/record
counts, and immutable torn-tail preservation. Per-phase synchronized receipts
make an exact publication retry idempotent after acknowledgement loss. This is
currently a storage primitive: until guardian runtime recovery rehydrates the
protocol state and owns the PTY write sequence, it is not live input-durability
or mux-crash continuity evidence.

The per-pane protocol state machine is finite and explicit:

```text
vacant
  -> live_unclaimed
  -> live_claimed(generation, mux_incarnation, next_sequence, pending_input_effect?)
  -> live_unclaimed                    [RetireLease or mux lease expiry]
  -> exited_unclaimed(exit_status, pending_input_effect?)
  -> closed_terminal(exit_status?)     [explicit Close after exit/retention]
```

`Spawn` is the only `vacant -> live_unclaimed` transition. `Claim` is the only
way to enter or replace `live_claimed`; it monotonically increments the fencing
generation. `RetireLease` handles an orderly mux handoff. Abrupt transport loss
uses an exact mux-incarnation retirement transition: unambiguous leases become
`live_unclaimed`, while a pane with `accepted_not_durable` input remains pinned
until its journal disposition resolves, after which retirement is retried. The
transition is idempotent, rejects nil identity, and cannot affect panes already
claimed by a successor incarnation. Child exit preserves census, replay, checkpoint, effect-query, and
retention-close authority but permanently rejects new PTY/process mutations.
An accepted input whose
durable/terminal disposition is still ambiguous survives child exit and blocks
lease takeover, retirement, and terminal close until that exact effect identity
is reconciled. `closed_terminal` is queryable through the bounded idempotency
window but cannot be claimed or spawned under the same durable pane UUID.
Full terminal output/checkpoint state may be compacted under the declared
retention policy, but the pane UUID and original spawn request/effect identity
remain as a durable tombstone. Reaching the pane/tombstone ceiling fails closed;
it never auto-evicts a tombstone to admit a new spawn. Reclamation requires a
separately authenticated explicit retention transaction with a durable receipt,
and a reclaimed UUID is never implicitly reusable as a new process identity.
Guardian incarnation rollover invalidates all transport sessions and requires a
fresh census; persisted pane generation never decreases. Exhaustion of a
generation, mutation sequence, output sequence, or idempotency counter is a
terminal quarantine condition, never wrapping arithmetic. Quarantine still
records a later child-exit status for census and forensics; it neither revives
mutations nor discards the exhaustion reason. An explicit close issued before
the child waiter settles likewise retains the later exit status instead of
losing it when the mutation authority becomes terminal.

These are protocol/state-machine requirements, not continuity evidence. The
implementation must first prove them with deterministic pure-state tests and
bounded-codec negative controls; only the later real PTY and SIGKILL lanes can
prove live-process survival.

A transactional mux upgrade may therefore stage and verify a same-build mux,
capture and verify a content dump, stop accepting new mux mutations, retire the
old mux lease, start the successor, claim/replay every guardian pane, verify
topology and content digests, and only then commit the service-manager pointer.
Before the guardian contract is live, the same command must fail closed when a
mux owns live PTYs; it may offer a verified content dump and a disruptive
restart, but it must never label that path lossless.

### Live mux content dump

`ft session dump` provides a separate, read-only pre-upgrade and forensic
artifact. It brackets sequential pane reads with two bounded pane listings and
requires their structural topology fingerprints to match. It captures redacted
pane metadata and redacted UTF-8 pane text into a versioned JSON envelope with
per-pane, whole-payload, and whole-artifact SHA-256 checksums. The output is
created through a no-symlink pinned directory capability with private
permissions, never overwrites an existing file, and synchronizes both the file
and containing directory before success is reported. Partial pane reads or
topology drift are recorded and fail the command by default. Payload and
topology checksum passes are streaming and byte-bounded. The topology projection
uses the same redacted canonical domain/workspace strings published in the
artifact, so its digest cannot retain a dictionary-testable pre-redaction value.
The command releases its producer-side pane-text tree before publication. After
durable publication, it also releases the artifact-sized serialization buffer,
rereads the private file through `verify-dump`, and compares the independent
producer/verifier receipts before reporting success. A rejected artifact
remains retained at its no-clobber path for diagnosis.
These unkeyed checksums detect corruption and internal inconsistency; they do
not authenticate origin against an actor who can rewrite the private artifact
and recompute every digest.

`ft session verify-dump <path>` performs bounded offline verification of the
private regular-file shape, complete-publication marker, schema, whole-payload
checksum, per-pane text checksums and byte/line counts, aggregate limits,
the one canonical v1 JSON encoding, exact field sets at every nested object,
pane metadata, exact outcome equality with the sorted unique initial-pane-ID
manifest, unique capture-error ownership, sorted domain summaries,
summary counters, topology-fence kind/scope/fingerprints, completeness/error
consistency, and the explicit non-restorable capability claims. A `complete:
true` artifact is accepted only when its initial/final pane counts match every
captured pane, its initial fingerprint recomputes from the canonical redacted
pane metadata, and its initial/final topology fingerprints agree. A valid artifact
can still have `complete: false` when it was deliberately retained with
`--allow-partial`; verification reports that state rather than promoting it to
a complete safety gate. Duplicate and unknown JSON members fail closed so
discarded or hidden bytes cannot coexist with a `redaction_applied` attestation.
The verifier also reapplies the current canonical redactor to every serialized
string value and requires the entire artifact to be a redaction fixed point. A writer
therefore cannot inject recognizable secret material, recompute every unkeyed
checksum, and retain a valid redaction attestation; rejection diagnostics never
echo the offending string.
Canonical comparison streams serializer output against the already bounded
artifact bytes; it does not allocate a second artifact-sized buffer. Before
the allocating JSON parse, a zero-retention streaming preflight applies global
node, map-entry, sequence-entry, decoded-string-byte, and nesting-depth limits,
preventing a byte-small structural payload from amplifying into an unbounded
value tree.

The dump is deliberately not accepted as an executable restore image. It does
not provide an atomic point-in-time content snapshot and does not preserve PTY
descriptors, process memory, shell/editor internal state, terminal parser/render
state, or running-agent continuity. Pane IDs are mux-incarnation-local unless a
source supplies stronger domain authority; the artifact records that provenance
instead of claiming stable cross-restart identity. `ft session recover
<pane_uuid>` likewise exports a redacted orphan transcript to a new file; it
does not replay archived output into a live pane or PTY.

Use `--dry-run`; `--layout-only` is currently a reserved no-op and the output
is only the bounded descriptor/status report:

```bash
ft snapshot restore 123 --layout-only --dry-run
```

## `ft restart`

`ft restart` execution is currently unavailable and fails closed before lock
acquisition, process discovery, snapshot capture, signaling, or any mux
mutation. The existing implementation cannot authenticate one exact mux
endpoint, process incarnation, and relaunch plan, so an acknowledgement flag is
not sufficient to make execution safe.

`--dry-run` reports this unavailable status and the intended continuity gaps.
It performs no operation. A future restart design must bind an authenticated
endpoint to an exact PID/incarnation receipt and a verified relaunch plan before
any stop/start workflow can ship.

Examples:

```bash
ft restart --dry-run
```

## Configuration

Snapshots are configured in `ft.toml` under `[snapshots]`:

```toml
[snapshots]
enabled = true
interval_seconds = 300
max_concurrent_captures = 10
retention_count = 10
retention_days = 7
```

Notes:

- In the library/test substrate, layout reconstruction creates a pane's default
  shell. Production CLI execution is unavailable, and the process layer never
  types a captured shell or agent command into a PTY.
- Captured shells and agents always receive an explicit manual disposition so
  state that was not restored cannot be mistaken for success.
- The retired `[snapshots.process_relaunch]` table is rejected with a migration
  error. Delete it from existing configuration; no replacement launch setting
  exists because process and agent restoration is unavailable.
- The entire top-level `[session]` table is unsupported and rejected. Delete
  it, including any retired `session.restore_max_lines` setting. Historical
  scrollback replay has no supported output channel, so there is no active
  replay-size limit to configure.
- Retention is enforced by both `retention_count` and `retention_days`.

## Performance budgets and proof status

Criterion budgets for isolated snapshot components live in
`crates/frankenterm-core/benches/snapshot_engine.rs`:

- Topology capture: **p50 < 1ms**
- Pane state extraction: **p50 < 10µs per pane**
- Dedup hash: **p50 < 100µs**
- SQLite transaction: **p50 < 10ms**
- SQLite query + deserialize: **p50 < 5ms**

These values are design budgets, not proof that the current release meets them
on a particular machine or at large topology sizes. End-to-end snapshot save,
checkpoint load, and library-restorer exercises also include mux transport,
SQLite authority transactions, topology size, and filesystem effects; there is
currently no production restore latency to quote. Do not infer a production
latency or scale support claim until the corresponding retained target-class
benchmark/soak artifact is non-skipped and signed.

These component caps also do not qualify long-history operator paths.
`ft session list` and `ft session show` now return bounded pages (`--limit 50`
by default, at most 200 rows, with `--offset` capped at 100,000) from one
read snapshot per invocation. They no longer materialize an entire history,
but list ordering, exact counts, and per-row clean-authority verification still
perform work that scales with the stored population; offset pages can also
drift between invocations. `ft session doctor` still scans and revalidates the
full history. Keyset/snapshot-token pagination, maintained authority summaries,
and a bounded doctor remain open work; do not infer large-history
responsiveness from the isolated snapshot budgets or the row-output caps.
