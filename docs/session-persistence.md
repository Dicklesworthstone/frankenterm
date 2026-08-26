# Session Persistence (Snapshots)

ft’s session persistence system captures terminal-backend mux evidence (current
bridge: WezTerm) into SQLite snapshots and can publish a portable, independently
verified artifact containing the checkpoint plus its retained redacted
scrollback prefixes. This lets you:

- Inspect the bounded metadata needed to plan a manual reconstruction after an unclean shutdown
- Preserve evidence before an operator-managed restart
- Inspect session state and compare pane-ID membership over time
- Export and verify a no-clobber forensic content artifact without a live mux

Snapshot capture/inspection and portable artifact export/verification ship.
Restore and restart execution do not: their non-dry CLI paths fail closed
before process or mux effects. The executable restorer is library/test
substrate, not a production recovery surface. This system preserves **mux
topology + pane metadata + a forensic retained scrollback prefix**; it is not
full process checkpointing.

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
- **Optional retained scrollback references**: per-pane segment identities and
  exact durable sequence bounds captured in the same checkpoint transaction.
  Manual CLI and Robot saves request these references by default. Periodic and
  shutdown capture paths also request them, so a later export can bind the
  retained prefixes to the exact checkpoint rather than reading an unbounded
  moving transcript.
- **Portable artifact projection**: the verified checkpoint identity and
  topology, redacted per-pane metadata, ordered durable scrollback segments,
  explicit gap/completeness facts, bounded aggregate counters, and independent
  payload/file SHA-256 receipts. `ft snapshot save` and Robot checkpoint save
  publish this artifact by default after the SQLite checkpoint commits.

The current topology schema v1 sorts numeric tab IDs for deterministic output.
It does not yet preserve user tab order or an incarnation-scoped active-tab
identity. The migration contract is
`docs/proposals/ft-7xqz4-8-10-1-tab-order-authority-contract.md`.

What it does **not** (currently) guarantee:

- Restoring interactive in-process state (REPL variables, editor buffers, etc.)
- Restarting shells, agents, or other foreground processes automatically
- Recovering content that was never captured, fell before a declared gap, or
  was outside the checkpoint's retained durable prefix
- Restoring terminal parser/render state from the exported scrollback bytes
- Preserving mux-domain identity, durable app-reopen tab order, titles, exact
  cell geometry, stable active-tab identity, window/workspace placement, or
  full window appearance
- Preserving a PTY descriptor, child process, shell/editor memory, or running
  agent merely because an artifact verifies

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
  "trigger": "manual",
  "artifact": {
    "operation": "save",
    "path": ".ft/checkpoint-artifacts/checkpoint-…json",
    "checkpoint_id": 123,
    "session_id": "sess-…",
    "checkpoint_role": "snapshot",
    "scrollback_complete": true,
    "restore_scope": "forensic_scrollback_and_checkpoint_topology_only",
    "running_process_continuity": false
  }
}
```

The one-shot save is ordered as checkpoint capture, durable artifact
publish-or-recover, offline verification, complete-prefix admission, then clean
session close. If publication acknowledgement is lost, the checkpoint remains
open and the error names `ft snapshot export <id>` as the reconciliation path.
An incomplete artifact is retained for forensics but the save fails rather
than promoting it to a pre-upgrade recovery checkpoint.

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

### 1a) Export, verify, and inventory portable artifacts

Export an existing checkpoint to the canonical workspace catalog:

```bash
ft snapshot export latest
ft snapshot export 123
```

Or choose one exact new output file:

```bash
ft snapshot export 123 --output /absolute/private/path/checkpoint-123.json
```

Existing files are never overwritten. Publication and acknowledgement recovery
bind six immutable fields: checkpoint ID, parent session ID, checkpoint
timestamp, checkpoint role, v2 state witness, and pane count. A retry first
loads those six fields from one SQLite row projection, then verifies the
existing artifact and requires all six fields to match before any new write. A
mismatched target fails closed without recapture or replacement. The underlying
publish-or-recover API also accepts a caller-retained six-field identity and
can reconcile an exact lost reply before opening SQLite; that is the path used
by a durable higher-level transaction. The standalone `ft snapshot export`
command still needs SQLite to resolve its requested checkpoint identity. Use
`verify-artifact` for database-independent artifact integrity inspection.
The explicit-path verifier dispatches before config, workspace, SQLite,
logging, fonts, or mux initialization, so recovery evidence remains inspectable
when the ordinary runtime state is damaged.
An explicit `list-artifacts --directory <path>` uses the same pre-initialization
offline boundary; the default directory form still resolves the configured
workspace catalog first.

Verify one artifact without opening SQLite, or inventory the bounded canonical
catalog:

```bash
ft snapshot verify-artifact /absolute/private/path/checkpoint-123.json
ft snapshot list-artifacts
ft snapshot list-artifacts --directory /absolute/private/catalog
```

The writer uses a private no-follow directory capability, mode 0600 and a
single link for the final file, create-new publication, file and parent
directory synchronization, canonical JSON, bounded parsing, and an independent
reread. Catalog inventory charges every directory entry against its finite
budget and fails closed if a canonical artifact is corrupt. An artifact with
an explicit gap remains verifiable but reports `scrollback_complete: false`;
`ft snapshot export` rejects it by default. `--allow-incomplete` is only an
operator acknowledgement for forensic salvage and never upgrades its
continuity capability.

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
offset becomes visible. Current v3 row records encrypt exact terminal semantics
and bind their framing and payload under guardian key authority; logical
retention pruning is recorded in a synchronized sequence journal so stable row
numbers survive a crash even before byte compaction.
Compaction writes the retained suffix plus its base-sequence header to a sibling
file, synchronizes it, atomically publishes it, and then collapses the journal.
The v3 identity manifest is AEAD-authenticated and records the next sequence
explicitly, including when retention leaves zero rows. Its canonical logical-
ledger SHA-256 binds generation and predecessor identity, durable pane and
versioned ledger identity, the exact sequence, row, and byte facts, and every
ordered length-delimited record. Constructor, reopen, snapshot, list, export,
replacement, and publication paths recompute the selected ledger before
trusting that digest. A private cross-process mutation lease and an on-disk
generation/digest compare-and-swap prevent two writers from replacing the same
authority. Reopen ignores torn content/journal tails, rejects complete malformed records,
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
a PTY. Export provenance distinguishes authenticated exact-semantic rows that
were intentionally encrypted without pre-persistence redaction, recognized
legacy `ftsl1`/`ftsl2` redact-before-encode framing that is not cryptographically
authenticated, and raw legacy rows whose pre-persistence redaction is unknown;
those categories account for every retained row. The older `ft session recover`
command remains specific to the separate 64-hex flat mmap orphan format; format
identity is never guessed from content. Recovery preserves whether decoding
reached the header-declared committed cursor. By default an incomplete source
or any skipped record is rejected before a transcript is created; an operator
must pass `--allow-partial` to retain that explicitly incomplete salvage, whose
structured result remains `complete: false` and includes the finite stop
reason. `ft session discard --force` removes only the still-leased,
identity-revalidated data leaf, synchronizes its pinned parent, and retains the
private lock inode to prevent split flock authority.

`list-orphans`, `recover`, and `discard` share caller-selectable finite limits
for directory entries, file bytes, records, replay chunks, payload bytes, and
transcript bytes. The defaults remain the conservative 64 MiB/50 MiB recovery
envelope, while explicit flags admit a non-default writer capacity only up to
the hard 1 GiB and 1,048,576-record ceilings. Canonical paired lock companions
have an independent bounded census so a first production scan cannot make a
second scan fail merely by creating the missing recovery leases. All
operational failures honor the requested plain/JSON/TOON format with stable,
bounded, redacted error codes and exit status 2.

Guardian raw-output v3 uses a keyed authenticated 176-byte header. A complete
legacy 160-byte v1 header is reported as unsupported and left byte-for-byte
untouched; it is never treated as a torn v3 header or shifted in place. Any
future legacy migration must conservatively read bounded legacy records into a
fresh v3 successor or a separately verified serialized artifact while retaining
the original evidence. Guardian input v3 similarly rejects ambiguous v1
fieldless `Durable` records rather than inventing full-write certainty, and it
rejects v2 logs because they lack the authenticated guardian-incarnation bind.

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
   directory. On supported Unix targets, each random 32-byte key is an
   immutable mode-0600, no-follow, single-link regular file whose lowercase ID
   is the first eight bytes of its SHA-256 fingerprint. Append-only,
   checksummed activation records start at
   generation one and remain contiguous. Each initial publication or rotation
   first synchronizes a deterministic private key stage, then atomically
   publishes an immutable 224-byte retained intent without replacement, then
   atomically renames the staged key to its final name without replacement,
   and finally stages and atomically publishes the activation without
   replacement. Each step synchronizes its file and capability-directory entry
   before the next step; a target without the required dirfd-relative atomic
   no-replace primitive refuses publication rather than falling back to an
   overwriting rename. The intent binds a random stable authority ID,
   generation, key ID and full key SHA-256, the exact predecessor generation,
   key ID and activation digest, the new activation digest, the predecessor
   intent digest, and its own domain-separated record digest. All intents from
   the first protocol generation onward are retained as one contiguous digest
   chain. A legacy activation-only authority transitions by binding its exact
   latest activation as the first retained intent's predecessor.

   Sibling-keyring creation first identity-pins the owned scrollback parent,
   creates and opens the private child relative to that descriptor, synchronizes
   the child and the pinned parent, and revalidates both descriptor-to-name
   bindings before and after keyring publication. Renaming or replacing the
   ambient scrollback path therefore fails closed instead of redirecting the
   parent sync or silently selecting an unsynchronized replacement child.

   Exactly one contiguous newest intent may lack its activation. A writable
   opener holds the exclusive authority lease across inventory, recovery, and
   publication and completes only that intent's exact staged or already-final
   key and activation. A read-only opener never performs recovery: when an
   earlier activation exists it may continue historical reads with that prior
   authority while reporting the pending generation, but it never authorizes
   the pending key; an initial pending generation without any activation is an
   explicit unavailable state. Private, canonically named stage residues are
   inert, size-checked, counted against the bounded directory inventory, and
   retained rather than deleted. A partial or unretained semantic final key,
   intent, or activation is fatal; it is never reclassified as an ignorable
   crash residue. Existing segment writers retain their original cipher until
   rollover, and historical keys are never overwritten or discarded while an
   activation can reference them. The pinned active key and full activation
   and intent inventory are revalidated before new cipher use or rotation, so
   external key mutation, forks, and concurrent advancement fail closed. Exact
   retry after publication acknowledgement loss reconciles the immutable bytes
   and then verifies the full contiguous inventory. A missing, truncated,
   symlinked, hard-linked, permission-unsafe, wrong-ID, or corrupt referenced
   key fails closed. Key bytes are zeroized when their in-memory authority is
   dropped and never enter `Debug`, argv, environment variables, receipts,
   dumps, or log fields. The retained chain detects missing or forked interior
   protocol records. Deletion of the entire retained-intent set can still make
   its surviving activations appear legacy, and deletion of an entire newest
   intent/activation/key suffix can expose its predecessor; both attacks remain
   outside this directory-only model. Detecting either rollback requires an
   independently durable monotonic head.

   The current standalone guardian runtime has a separate authority at
   `guardian-output-v3/journal.key`. Its static 32-byte key now reuses the
   guardian token publisher's private stage, digest-bound readiness marker, and
   atomic no-replace publication. If the final key is absent, a bounded,
   identity-revalidated artifact census permits only recognized provisioning
   residue and refuses to create a split authority when any output artifact is
   already present; a legacy partial semantic final remains fatal. This is not
   yet the activated mux keyring described above: it has no rotations,
   historical key inventory, retained intent chain, or shared active-key
   integration.

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

#### Canonical terminal checkpoint inventory (current v2)

The terminal payload is a semantic model, not a dump of Rust struct memory.
Its canonical encoding uses fixed field order and sorted map keys so equivalent
state has one byte representation and one digest. The current version must
cover all of the following before it can be published:

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

#### Guardian protocol v4 wire contract and activation status

The current protocol version is v4. It uses one length-delimited binary frame
with a hard encoded-frame ceiling. Authentication is an
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
generation; it cannot skip, wrap, or revive a terminal pane. The base `Attach`
reply acknowledges only the exact pane, generation, and next mutation sequence;
that identity-only reply is not continuity evidence. Protocol v4 now defines
bounded typed codecs for content-addressed `CheckpointStage` begin/chunk/seal
requests and replies, `Checkpoint` publication intents and receipts, paginated
`Replay` requests and pages, and cumulative `ReplayAck` requests and receipts.
Those are wire and pure-state surfaces, not a live checkpoint service. The
standalone guardian runtime still rejects `Checkpoint`, `CheckpointStage`,
`Replay`, and `ReplayAck` instead of dispatching them. Durable checkpoint
artifact storage/publication, runtime checkpoint dispatch, the replay service,
retention-watermark advancement, and automated live migration remain
unimplemented and withheld.

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
sequence gap, or exhausted sequence before performing an effect. Input effect
queries distinguish `not_seen`, `accepted_not_durable`, `durable_full`,
`durable_prefix { applied_bytes }`, `known_not_applied`, and
`disposition_unavailable`. `NotSeen` alone authorizes the original first
attempt; `KnownNotApplied` is a terminal proof that zero bytes were applied,
while `disposition_unavailable` requires operator/runtime reconciliation
without replay. A newly accepted input acknowledgement can contain only
`accepted_not_durable`, `durable_full`, a valid exact `durable_prefix`, or
`known_not_applied`, never either query-only state. At the
protocol/state-machine layer, resize, signal, close, and checkpoint operations
have the same request-digest replay rule, so an ambiguous response is queried
or replayed by identity rather than converted into a second effect. This
checkpoint rule does not imply runtime checkpoint dispatch.

The protocol contract requires runtime effects to pass through a transactional
API rather than the pure observation surface. Authentication, incarnation,
idempotency, capacity, pane state, generation, and sequence must be validated
before an external effect. All receipt maps, reverse indexes, protected
identities, and queue capacity must be reserved before that boundary, and an
exact request or effect replay must return its retained receipt without
invoking the effect again. Callback failure is permitted only when no bytes,
signal, resize, process, or other external effect became observable. A possibly
partial input write is therefore committed as `accepted_not_durable` and
reconciled by its exact effect UUID; it must never be reported as a safely
retryable callback failure.

The typed input and checkpoint protocol primitives implement that stronger
pre-reserved commit pattern. Live guardian `Input` now takes the owned transport
path: the authenticated request and its exact connection-generation,
request-ID, and effect-ID route move to one fixed worker through a capacity-one
queue. That worker temporarily owns the protocol state, target pane writer, and
descriptor-pinned encrypted input journal; it synchronizes admission before the
one PTY write, records the exact outcome, wipes plaintext, and returns a delayed
completion to the readiness loop. The loop delivers that completion only to the
still-matching connection route. Saturation or an unavailable pre-submit worker
or pane authority closes retryably before submission, without a write or a
fabricated terminal rejection; a panic or indeterminate completion quarantines
the effect instead of authorizing a retry.
The borrowed `dispatch` path still rejects `Input` so transport cannot bypass
this owned continuation; `input_activation_rejections` counts that invalid
bypass, not ordinary live input activation. The generic in-memory transaction
path for `Spawn`, `Resize`, `Signal`, live-pane `Close`, `Claim`, and
`RetireLease` pre-reserves its receipt maps, reverse indexes, protected
identities, and queues and installs a conservative pane quarantine before
invoking the external callback. A definitely-not-applied result restores the
exact prior pane state; an applied result commits the already-reserved receipt;
and a panic or indeterminate result retains both the exact effect identity and
a non-retryable quarantine. Exact request/effect replay therefore cannot invoke
the callback a second time merely because an in-process allocation or callback
panic occurred. These are current source paths, not executed restart or
migration evidence.

That is still only live guardian-process authority, not crash durability. The
generic effect identities, receipts, and quarantine transitions are not yet
synchronized to a restart-recoverable protocol journal before the external
boundary, and each runtime adapter must still prove that every error it labels
definitely-not-applied really has zero observable effect. Response construction
after a successful commit must also close the connection for exact receipt
replay rather than emit a terminal negative acknowledgment. Guardian restart
recovery and mux-upgrade continuity remain withheld until durable generic effect
recovery, operation-specific reconciliation of ambiguous outcomes, and
mutation-sensitive crash/fault cuts prove that no observable effect can execute
twice across guardian restart.

The guardian input-effect journal is the live `Input` storage primitive that
synchronizes transaction evidence without persisting raw keystrokes. Before it
can publish a new Intent, it reserves record, byte, and sequence capacity for the
complete Intent + `accepted_not_durable` + terminal lifecycle, including every
follow-up already promised to another incomplete effect. Capacity exhaustion
therefore occurs before a write permit can exist. For each admitted input the
primitive synchronizes the encrypted exact identity and then a conservative
`accepted_not_durable` marker before any PTY write that could expose bytes. Only
a newly committed Accepted record yields the opaque one-shot PTY-write permit;
reconciliation of an exact, alias, or stale retry returns the current
disposition and cannot authorize another write. Raw WAL append and
permit-conversion APIs are mux-crate-private; the guardian uses only the public
transaction and outcome-commit seams. A definitely zero-byte result
may refine that marker to `known_not_applied`; a proven complete result becomes
`durable_full`; a proven partial result records the exact nonzero
`durable_prefix { applied_bytes }`;
otherwise it remains conservatively pending. Total input length and applied
count are authenticated and flow through opaque journal-backed protocol
completion authority, so journal and reply cannot disagree. The payload
SHA-256 is encrypted rather than written as plaintext because low-entropy key
events are dictionary-testable. The input log uses an input-domain-separated
AEAD surface of the activated guardian journal key; rotation must retain an old
key generation while either an output segment or input log still references
it. The v3 header binds the exact nonzero guardian incarnation, and every v3
record includes that incarnation and durable pane in both its AEAD associated
data and chained outer digest. A journal transplanted from another incarnation
therefore fails before recovery. Each fixed-size v3 record authenticates its
clear framing and predecessor digest, and scanning enforces monotonic journal
order,
lease-generation input order, exact legal transitions, bounded effect/record
counts, and immutable torn-tail preservation. Per-phase synchronized receipts
make an exact publication retry idempotent after acknowledgement loss. The
standalone runtime now opens this descriptor-pinned journal per pane and routes
live `Input` through the bounded owned-input worker/continuation pipeline.
Restart recovery additionally requires a durable anti-rollback high-water
authority: accepting only a valid file prefix is insufficient because removal
of a terminal suffix could otherwise make executed input appear safely
unapplied. Accordingly, a scanned Intent maps to
`disposition_unavailable`, every reopened log (including a valid header-only
prefix) withholds append authority, and only exact idempotent receipt reads
remain available. Live input inside the current guardian process is therefore
implemented, but guardian-restart recovery and mux-crash continuity are not.

The older v1 journal first appeared after the latest tagged release and is not
wired by any released runtime caller. Its fieldless `Durable` state cannot prove
full versus partial application, so v3 scanning rejects it as ambiguous. The v2
format has exact dispositions but no authenticated guardian-incarnation binding,
so v3 also preserves and rejects it. If an untagged build produced either file,
it must be quarantined or migrated conservatively under an offline authority;
it must never be promoted to `durable_full` merely because its prefix parses.

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
claimed by a successor incarnation. The pure state machine preserves the
identities required for future replay, checkpoint, effect-query, and
retention-close authority after child exit, while permanently rejecting new
PTY/process mutations. The current runtime exposes census and effect-query
behavior but does not yet dispatch checkpoint, replay, or retention advancement.
An accepted input whose
durable/terminal disposition is still ambiguous survives child exit and blocks
lease takeover, retirement, and terminal close until that exact effect identity
is reconciled. `closed_terminal` is queryable through the bounded idempotency
window but cannot be claimed or spawned under the same durable pane UUID.
A future retention service may compact full terminal output/checkpoint state
under the declared policy, but it must retain the pane UUID and original spawn
request/effect identity as a durable tombstone. No live retention advancement
or reclamation transaction is implemented today. The state-machine ceiling
fails closed rather than auto-evicting a tombstone to admit a new spawn; future
reclamation requires a separately authenticated explicit transaction with a
durable receipt, and a reclaimed UUID must never be implicitly reusable as a
new process identity.
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

An eventual transactional mux upgrade may stage and verify a same-build mux,
capture and verify a content dump, stop accepting new mux mutations, retire the
old mux lease, start the successor, claim/replay every guardian pane, verify
topology and content digests, and only then commit the service-manager pointer.
That automated live-migration transaction is not implemented. Until durable
checkpoint storage, checkpoint runtime dispatch, replay, retention advancement,
and restart recovery are all live and proven, an upgrade command must fail
closed when a mux owns live PTYs; it may offer a verified content dump and a
disruptive restart, but it must never label that path lossless.

### Live mux content dump

`ft session dump` provides a separate, read-only pre-upgrade and forensic
artifact. It brackets sequential pane reads with two bounded pane listings and
requires their structural topology fingerprints to match. It captures redacted
pane metadata and redacted UTF-8 pane text into a versioned JSON envelope with
per-pane, whole-payload, and whole-artifact SHA-256 checksums. The output is
created through a no-symlink pinned directory capability. Directories the
command creates use mode 0700 and the artifact itself uses mode 0600; a custom
pre-existing parent is not silently chmodded. Both producer and verifier reopen
the complete parent path without following symlinks and require its device and
inode to match the pinned capability, so a renamed/replaced parent cannot turn
a detached file into a successful pathname receipt. The artifact requires one
and only one hard link to its inode, is never overwritten, and both the file
and containing directory are synchronized before success is reported. Partial
pane reads or topology drift are
recorded and fail the command by default. Payload and
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
private single-link regular-file shape, complete-publication marker, schema,
whole-payload checksum, per-pane text checksums and byte/line counts, aggregate limits,
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
a complete safety gate. Pass `--require-complete` for any upgrade or recovery
admission check; it rejects an otherwise intact partial artifact or any nonzero
capture-error count. Duplicate and unknown JSON members fail closed so
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

#### Exact v0.13 compatible-client bridge

The v0.13.0 release predates `ft session dump`. A newer candidate must not try
to speak its incompatible wire dialect directly to that old mux. Instead,
`ft session compatible-client-dump` runs the exact retained v0.13.0 `ft`
executable as a bounded external client against one explicit live Unix socket:

```bash
candidate-ft session compatible-client-dump \
  --client /absolute/path/to/v0.13.0/ft \
  --expected-client-sha256 <64-lowercase-hex> \
  --expected-client-bytes <exact-length> \
  --mux-socket /absolute/path/to/live/gui-socket \
  --output /absolute/private/path/pre-upgrade.json \
  --max-panes 1024 \
  --max-total-bytes 67108864 \
  --batch-size 8 \
  --batch-timeout-secs 30 \
  --max-batch-output-bytes 33554432 \
  --format json
```

The bridge requires version `0.13.0` and git identity `3ebd60566`, pins the
client and socket identities without following symlinks, creates a sterile
private environment, obtains two state censuses around bounded batches of
`robot get-text`, and publishes through the ordinary dump verifier. The
topology contract contains only facts available in the v0.13 Robot projection:
numeric pane/tab/window IDs, optional pane UUID, and redacted domain. It does
not claim workspace, geometry, active/zoom, stable mux incarnation, or
authoritative domain identity.

Before contacting either the old client or mux, an exact-path retry performs an
offline Query/Ack. The existing complete artifact must match the expected
client hash/length/version, socket-path digest, and every request bound. Exact
recovery re-synchronizes and acknowledges the retained artifact; mismatch,
corruption, or an incomplete capture fails without overwrite or recapture.
Both fresh and recovered receipts expose a verifier-derived, sorted
`domain_pane_counts` map. An upgrade coordinator can therefore require named
domain coverage without trusting producer counters or reparsing unverified
JSON.

```bash
candidate-ft session verify-dump /absolute/private/path/pre-upgrade.json \
  --require-complete \
  --expect-domain-panes 'ssh:example-a=12' \
  --expect-domain-panes 'ssh:example-b=16' \
  --format json
```

Each expectation is exact, repeatable, bounded, and evaluated only against the
offline verifier's pane records. `--require-complete` additionally admits only
a complete zero-error capture. Missing domains, mismatched counts, duplicate
expectations, zero counts, malformed values, and partial captures fail closed.
`verify-dump` dispatches before configuration, workspace, database, logging,
font, or live-mux initialization, so damaged local state cannot block or mutate
this offline incident-recovery check.

The private sterile environment is retained as reconciliation evidence. A
successful receipt still declares `forensic_text_export: true`,
`executable_restore_image: false`, and `production_mux_activation: false`.

The dump is deliberately not accepted as an executable restore image. It does
not provide an atomic point-in-time content snapshot and does not preserve PTY
descriptors, process memory, shell/editor internal state, terminal parser/render
state, or running-agent continuity. Pane IDs are mux-incarnation-local unless a
source supplies stronger domain authority; the artifact records that provenance
instead of claiming stable cross-restart identity. `ft session recover
<pane_uuid>` likewise exports a redacted orphan transcript to a new file; it
does not replay archived output into a live pane or PTY. It refuses an
incomplete or record-skipping export unless `--allow-partial` is explicit and
never labels an opted-in salvage complete.

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
