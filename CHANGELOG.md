# Changelog

All notable changes to FrankenTerm (`ft`) are documented in this file.

Organized by landed capabilities, not raw diff order. Each section describes what shipped and why it matters. Commit links point to the canonical GitHub repository at <https://github.com/Dicklesworthstone/frankenterm>.

- **Default branch**: `main`
- **Tags & GitHub Releases**: listed under [Tags & Releases](#tags--releases). Every `v0.2.0`–`v0.15.0` tag has a published GitHub Release; `backup-before-rewrite` is a tag only. The `v0.15.0` release was source-only and was superseded by the complete `v0.15.1` platform release.

Scope window: [v0.12.0](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.12.0) (2026-06-29) through [v0.15.1](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.15.1) (2026-08-21). The previously omitted [v0.13.0](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.13.0) GitHub Release (published 2026-07-28) remains a first-class version row.

## Version Timeline

`Kind` distinguishes a published GitHub Release from a plain git tag. Full spine is under [Tags & Releases](#tags--releases).

| Version | Kind | Date | Summary |
|---------|------|------|---------|
| [Unreleased](https://github.com/Dicklesworthstone/frankenterm/compare/v0.15.1...main) | HEAD | — | Next release |
| [v0.15.1](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.15.1) | Release | 2026-08-21 | Complete platform artifacts and macOS GUI installation |
| [v0.15.0](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.15.0) | Release | 2026-08-20 | Sampled paste tracing over additive PDU99 |
| [v0.14.1](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.14.1) | Release | 2026-08-20 | Pane-input argv privacy and release-contract repair |
| [v0.14.0](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.14.0) | Release | 2026-08-20 | Mux authority, scheduler admission, recorder truth, and protocol hardening |
| [v0.13.0](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.13.0) | Release | 2026-07-28 | Test-suite honesty + tx/capture/redaction; full platform matrix |
| [v0.12.0](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.12.0) | Release | 2026-06-29 | asupersync 0.3.5 churn fix + window-maximize persistence |

---

## [Unreleased] -- development on `main` since [v0.15.1](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.15.1)

Compare: <https://github.com/Dicklesworthstone/frankenterm/compare/v0.15.1...main>

### Reconnect and process-family deployment safety

- Cancels every parked main-thread runnable when its owner-thread executor is
  dropped, breaking a scheduler/queue reference cycle that could leak detached
  futures and their admission permits across mux test or service lifetimes.
  Queue retirement is synchronized with enqueue so a producer that reserved
  capacity before shutdown cannot publish a runnable after the final drain.
  The executor is now owner-thread affine in the type system because its queue
  may contain `spawn_local` state.
- Makes terminal domain reconnect failures visible and actionable, including
  both codec windows and explicit same-release server-upgrade or client-rollback
  guidance instead of an apparent no-op. Auto-connect attempts are independent
  and lazily bounded-concurrent: a failed first domain cannot suppress later domains
  or normal GUI publication, and initially unavailable domains retry with
  generation-aware jittered backoff. The first retry generation is fenced on
  successful initial GUI topology publication, so an early config reload cannot
  race reconnect work against an unfinished or failed startup. The existing
  bootstrap deadline now encloses transport dialing as well as codec/topology
  RPCs, so a stalled SSH/TLS/unix connect cannot permanently occupy all bounded
  auto-connect slots and starve later domains. Explicit or
  configured-default remote
  failures populate the published window with a local recovery shell while
  compatible auto domains continue retrying. Command-palette attach and spawn
  failures now emit the same bounded, terminal-sanitized, secret-redacted
  durable diagnostic instead of appearing to do nothing or copying an
  unbounded error into a toast. Fleet-wide auto-connect outages retain one
  diagnostic per failed domain but coalesce the interactive notification, so a
  large failure set cannot create a persistent-toast storm. Each desired
  domain now owns its own health discovery, failure count, retry deadline, and
  capped backoff, so a long-backoff `trj` cannot delay a newly detached `csd`.
  The reconnect manifest is loaded once on a persistence worker and thereafter
  updated in memory by durable lifecycle events rather than reread on every
  health pass.
- Remembers explicit remote-domain attachment intent across GUI restarts in a
  bounded, checksummed two-slot manifest. Domain aliases are persisted only as
  domain-separated SHA-256 fingerprints; the authority directory is mode 0700,
  pinned by descriptor identity, and revalidated against its configured name;
  lock and slot files are opened relative to that capability as mode 0600,
  no-follow, single-link, owner-matched leaves and revalidated after I/O before
  the synchronized generation becomes authoritative. A remembered
  attach overrides a false `connect_automatically` default, a remembered detach
  suppresses the built-in supervisor, and an absent record continues to follow configuration. Manual,
  command-palette, and Lua attach/detach paths durably publish intent before
  mutating the mux, while a torn inactive slot falls back to the last verified
  generation. Failed command-palette and Lua attaches release their exact
  single-flight transport claim before rebuilding supervision with the newly
  remembered domain, and Lua detach fences the retired retry generation before
  mutating the live domain. A failed explicit open reports automatic retry only
  after durable intent and an enabled startup handoff, while live command-palette
  and Lua failures require that exact attachment to enter the scheduled retry
  frontier. Attach, detach, auto-connect, startup, and config-reload actions are
  ticket-ordered per exact domain name across persistence, mux mutation, and
  retry handoff; cancellation advances the queue without allowing a later
  same-name action to overtake an active predecessor. A successful remembered
  attach also refreshes the long-lived supervisor plan, so later exhaustion of
  the transport's internal reconnect budget cannot silently drop it. Config
  reload reconciles every independent domain/default while exact old
  generations drain, retries same-name client-to-raw transitions, and fences
  stale or scheduler-rejected reload generations. Main-thread admission
  saturation is retained by one serialized `Idle`/`Starting`/`Running`
  coordinator; only a successfully created worker counts as a retry handoff,
  and its retirement is linearized with newer request publication so neither
  thread-creation failure nor a cross-atomic lost wakeup can turn a promised
  reconnect into a no-op. A config reload publishes a fail-closed validation
  gate before revoking both the old admission request and supervisor epochs.
  Automatic dialing cannot consume that config until the exact newest
  generation passes aggregate reconciliation; invalid, duplicate, stale, or
  terminally rejected generations remain visibly blocked until a later valid
  reload converges. Corrupt optional state fails visibly and leaves only explicit
  `connect_automatically = true` configuration eligible for automatic dialing.
- Keeps attached remote-domain reconnect supervision alive indefinitely by
  default with capped backoff and one reused connection window. The finite
  dial/cycle retry budget remains available only as an explicit operator
  opt-in, so a long outage no longer converts the default supervisor into
  permanent detach. Backoff doubling saturates before applying the configured
  ceiling, so even an extreme duration setting cannot overflow and panic the
  supervisor. Configuration loading now rejects a zero base delay, a maximum
  below the base, and a zero healthy-session fence; those values previously
  enabled a zero-delay CPU spin, a backwards backoff jump, or accidental
  defeat of an explicitly finite cycle budget. A transient topology-scheduler admission rejection after
  transport connect now fences that exact successor generation and routes it
  through ordinary retirement/retry instead of silently terminating the
  unlimited supervisor.
- Ships `ft` and `frankenterm-mux-server` as one sealed release identity and
  prevents remote setup from activating one side while an incompatible mux is
  still live. Verified local and release-tag pairs are staged under matching
  unique `pending-*` paths until a deliberate drained activation. Manual and
  preserved mux generations also trip the live-owner fence, which suppresses
  `systemctl enable --now` so setup cannot start a second generation.
  Inactive-host release pairs are likewise downloaded and launch-probed in an
  isolated directory before transactional canonical publication with rollback.
  The top-level installer also launch-probes both staged components before
  moving either installed binary to a backup name.
- Binds both local-binary and pinned-release installs to exact size/SHA-256
  receipts, revalidates those bytes while moving into the randomized stage and
  immediately before each pending or active rename, and emits the exact
  transaction UUID in the activation receipt. Remote setup also refuses
  receipt-authorized mutation unless a preflight marker proves the SSH command
  channel has uncontaminated stdout. Binary backups, failed-publication
  quarantine names, and service-unit staging/backup names use an exact unique
  transaction identity and refuse collisions before moving active paths.
  Local uploads require an exact nonexistence preflight receipt, stream the
  already-open no-follow source descriptor through an SSH no-clobber create,
  and recheck local identity plus remote digest before publication. Inactive
  publication refuses symlink or non-regular canonical binaries and service
  units. Service inspection likewise uses a typed shape marker and
  never opens a symlink, directory, device, or FIFO. The live-mux process probe
  also avoids matching its own
  shell command, distinguishes a true no-match from probe failure, and lets an
  inactive host reach transactional publication rather than being permanently
  misclassified as a live-owner host. Canonical remote publication holds an
  advisory lock on the installation-directory descriptor, restores preserved
  components only into absent paths, and emits an explicit incomplete-rollback
  marker instead of overwriting a concurrently occupied target. Every publish,
  backup, quarantine, and restore rename is itself no-clobber and verifies that
  the source disappeared and destination materialized, closing the race between
  an absence precheck and a plain overwriting `mv`. The pinned-release cache-to-stage
  binding holds that same directory lock, uses the same verified no-clobber move,
  and reports a distinct incomplete rollback if its first component cannot be
  returned to the release cache.
- Hardens the top-level installer process-family transaction as well: both
  package sources and existing canonical binaries must be regular non-symlink
  files, every stage/backup/quarantine name must be absent including dangling
  symlinks, and an apparent already-installed pair cannot bypass those shape
  fences through symlinked executables. Rollback restores only into an absent
  canonical path, checks every preservation move, and reports incomplete
  recovery instead of silently ignoring a second filesystem failure.
  FrankenTerm.app staging, backup, publication, and rollback use the same
  no-clobber discipline and reject dangling symlinks or non-directory targets.
  The already-installed fast path now runs inside the installer lock and
  requires one identical sealed build identity across `ft` and the mux server;
  matching semver strings from different builds can no longer skip repair. The
  shared marker must also name the resolved target, `release-interactive`
  profile, and requested version, so a mutually consistent stale, debug, or
  translated-target pair cannot suppress repair. Its version/marker pipeline
  prerequisites fail closed before either installed component executes. A
  shell regression test covers exact-pair admission, mismatched-build,
  stale-version, non-shipping-profile, wrong-target, and conflicting
  multi-marker rejection.
- Automates a verified pre-upgrade content dump through the currently installed
  codec-compatible remote CLI whenever that release supports `session dump`;
  legacy clients emit an explicit unavailable warning without fabricating proof.
- Documents the breaking-codec rollout order so the compatible old desktop
  process remains active while a separately installed candidate CLI stages and
  drains every remote server.

### Reliable pane-input delivery

- Adds negotiated codec v64 PDU100/101 for bounded arbitrary pane bytes on the
  same ordered client lane as reliable key transitions. `PaneWriter::write`
  now transfers ownership only after a shared FIFO accepts a bounded prefix;
  the in-flight entry continues to count against hard event and byte caps.
- Performs at most one remote `Write::write` effect per exact serial,
  pane-registration identity, payload length, and SHA-256 payload digest. Typed
  replies distinguish exact applied prefixes (including ACK-loss replay),
  definitely-zero retries or rejections, and indeterminate outcomes that are
  quarantined rather than duplicated.
- Keeps a partially applied suffix ahead of every later queued key or write
  under its pre-reserved fresh serial, fences ambiguous retries across mux
  server incarnation changes, and makes `flush` wait off the mux thread without
  retaining the queue lock. A mux-thread flush reports persistent `WouldBlock`
  while accepted work remains pending, and terminal delivery failures remain
  sticky and user-visible.
- Refuses pre-v64 peers explicitly instead of falling back to legacy
  `WriteToPane`. The existing matched-pair upgrade flow is the supported path.
  The delivery ledger remains mux-process-local; crash-durable accepted-input
  replay is intentionally unclaimed until the guardian input WAL lands.

### Session and scrollback durability

- Adds `ft session dump` plus bounded offline `verify-dump` for private,
  redacted, checksummed live pane-text and topology safety artifacts. The schema
  explicitly refuses to claim PTY, process-memory, or executable restore state.
  Compact payload hashing and pretty envelope serialization both write through
  a hard byte ceiling, so JSON escape expansion fails before exceeding the
  artifact memory envelope rather than being checked only after allocation.
  Payload verification and topology fingerprints use bounded streaming hashes.
  The topology projection hashes the same redacted canonical domain/workspace
  strings published in the artifact rather than retaining a dictionary-testable
  digest of pre-redaction values. The frozen v1 verifier requires one canonical pretty
  encoding plus exact field sets for the envelope, payload, metadata, content,
  topology, and each error variant; duplicate or unknown JSON members cannot
  smuggle discarded unredacted bytes behind a valid recomputed checksum. The
  verifier independently reapplies the canonical redactor to every serialized
  string value and requires a fixed point, so a checksum-valid writer cannot inject a
  recognizable secret behind a truthful-looking redaction flag; rejection
  diagnostics do not echo the offending material. The
  canonical comparison is streamed against the bounded input rather than
  allocating a second artifact-sized output buffer. Before the allocating JSON
  parse, a zero-retention streaming preflight enforces global node, map-entry,
  sequence-entry, decoded-string-byte, and nesting-depth ceilings, so a
  byte-bounded artifact cannot amplify into an unbounded in-memory value tree.
  Offline verification also checks the pane metadata schema,
  source/consistency contract, exactly one capture-or-error outcome per
  initially listed pane, exact equality with a sorted unique initial-pane-ID
  manifest, unique capture-error ownership, aggregate-limit
  arithmetic, captured-domain inclusion, sorted domain summary, topology-fence
  kind and scope, recomputation of complete-capture fingerprints from canonical
  redacted pane metadata, canonical initial/final fingerprints, and agreement among
  the completeness flag, topology stability, counts, and recorded errors; a
  checksum-valid but internally false continuity claim is rejected. `session
  dump` releases its producer-side pane-text tree before publication, then
  releases the artifact-sized serialization buffer and runs that same offline
  verifier against the durably published file before it can emit success,
  comparing the independent verifier and producer receipts; an
  invalid artifact is retained for diagnosis but never reported as a valid
  pre-upgrade safety gate.
- Carries stable pane UUIDs into mux panes and continuously persists evicted
  styled scrollback rows with synchronized sequence authority, checksummed
  manifests and records, torn-tail recovery, bounded compaction, and optional
  erasure sidecars.
- Makes mmap and SQLite fallback a durable per-pane authority decision instead
  of an uncoordinated dual write, and retains rows in memory when cold storage
  cannot acknowledge them.
- Adds `ft session list-durable` and `export-durable` for bounded read-only
  discovery and transcript export of the continuous format. Export refuses
  symlink, privacy, checksum, identity, sequence, replacement-race, and resource
  violations; excludes uncommitted torn tails; reapplies redaction; never opens
  source content for write; and never sends bytes into a live PTY. Its result
  separately accounts for authenticated exact-semantic rows encrypted without
  semantic-destroying pre-persistence redaction, unauthenticated legacy
  redact-before-encode framing, and raw legacy rows with unknown redaction
  provenance, rather than promoting every legacy byte to a proven privacy claim.
- Introduces the guardian raw-output journal substrate with mandatory
  XChaCha20-Poly1305 encryption, pane- and segment-bound record digests, strict
  byte/record admission caps, synchronized append receipts, poisoned recovery
  after ambiguous I/O, and read-only preservation of incomplete crash tails.
  Segment headers enforce a globally contiguous predecessor
  segment/sequence/digest chain, and a newly created segment cannot accept
  output before both its file and parent directory entry are synchronized.
  Adds the descriptor/capability-based encryption-key authority: fallible OS
  entropy, zeroizing in-memory keys, private no-follow single-link key files,
  contiguous immutable activation generations, crash-safe append-only rotation,
  and historical-key lookup for old segments. Referenced weak, truncated,
  symlinked, hard-linked, permission-unsafe, missing, or identity-mismatched keys
  fail closed; an incomplete unactivated rotation leaf is preserved without
  superseding the last durable activation and cannot be selected by a forged
  segment key ID. The pinned active key and complete activation inventory are
  revalidated before cipher use and rotation, so same-process authority cannot
  silently survive external key mutation or a concurrent activation advance.
  An exact activation retry after acknowledgement loss reopens and synchronizes
  the immutable record, referenced key, and full contiguous inventory before
  accepting the already-published generation; partial or conflicting records
  remain terminal failures. Current v3 files use a keyed, authenticated
  176-byte header; a complete legacy 160-byte v1 header is recognized as an
  unsupported preserved artifact, never misclassified as a torn v3 file or
  shifted or rewritten in place. Any future legacy recovery must emit a fresh
  v3 successor or a separately verified serialized artifact. V3 cold-scrollback
  manifests also authenticate a canonical logical-ledger SHA-256 over the
  generation, predecessor, pane/ledger identity, exact sequence/byte facts,
  and every ordered length-delimited record. Publication and export recompute
  that digest, while a private cross-process mutation lease plus on-disk
  generation/digest comparison prevents stale-writer replacement.
  This is storage/format substrate only; it is not yet wired to a live guardian
  PTY reader and does not claim mux-crash process continuity.
- Adds the guardian input-effect journal substrate. It synchronizes an encrypted
  exact intent and a conservative `AcceptedNotDurable` marker before a PTY write
  may become observable, then permits only `DurableFull`, an exact nonzero
  `DurablePrefix { applied_bytes }`, or `KnownNotApplied` as a terminal
  refinement. Total input length and applied-prefix count are authenticated, so
  a partial write can never be promoted to full durability or replayed from byte
  zero. Only a newly synchronized Accepted transition yields the opaque
  one-attempt PTY-write permit; an exact, alias, or stale retry returns the
  current reconciled disposition without fresh write authority. Fixed-size
  records encrypt the mux, generation, sequence, effect, payload-fingerprint,
  and input-length identity under a separate AEAD domain; raw keystrokes are
  never stored. Record and effect
  limits bound recovery memory and disk growth, digest chaining detects
  reordering, exact per-phase receipts reconcile acknowledgement loss, and
  complete corruption, torn tails, external writes, and ambiguous I/O fail
  closed without truncating evidence. This remains a descriptor-level storage
  primitive until the guardian PTY runtime and takeover recovery path are wired.
  Its v2 format fails closed on fieldless v1 `Durable`, because old bytes cannot
  prove whether the entire input or only a prefix became observable.
- Adds an explicit escape-parser recovery-ground authority and versioned parser
  checkpoint identity. The authority rejects partial CSI, OSC, DCS, Sixel,
  termcap, UTF-8, and tmux-control state even when the underlying VT transition
  table alone appears idle. A terminal checkpoint may therefore bind a fresh
  parser only to the exact processed raw-output sequence at which this stricter
  predicate was true; older output offsets paired with newer screen state are
  forbidden because suffix replay would duplicate terminal effects. This is a
  checkpoint substrate only; production guardian publication remains pending.
  The current v2 capability-free terminal projection covers both screen buffers and saved
  cursors plus the complete performer, mode, margin, keyboard, mouse, palette,
  metadata, Unicode-width, bidi, focus, geometry, and sequence state. It sorts
  terminal maps, deduplicated custom-width tables, and hyperlinks for canonical
  encoding, carries authenticated exact cold-scrollback rows, and rejects
  out-of-band graphics until their bounded representation exists. OSC-8 hyperlink serialization also
  now sorts its internal parameter map, closing a pre-existing path where
  identical styled lines could produce different checkpoint/render digests.
  Strict structural and semantic decode bounds, canonical re-encoding,
  off-topology inert restore, suffix-replay resource bounds, configuration
  fencing, and staged scrollback activation now fail closed. A live mux parser
  barrier can capture the model only at the exact durable receipt watermark and
  retains its own pane-generation lease through serialization; this remains
  source substrate until the guardian runtime durably publishes and reattaches it.
- Freezes the guardian v3 authenticated request/response envelopes and pure
  fencing state machine: bounded HMAC-before-decode framing, exact response
  correlation, a token-authenticated payload-free `Hello` bootstrap that is
  the sole nil guardian-incarnation scope and returns the current nonzero
  incarnation in a fixed typed payload, durable pane UUIDs, immutable
  byte-capped census snapshots,
  guardian-allocated snapshot identities (nil only as the first-page request
  marker) so a rotated UUID can never recreate a different pane view,
  monotonic lease generations and mutation sequences, idempotent effect
  identities, ambiguous-input reconciliation, exit-time replay authority, and
  terminal exhaustion quarantine. An exact mux-incarnation disconnect
  transition retires unambiguous leases only after the transport proves its
  final authenticated connection gone, pins ambiguous input until journal
  reconciliation, and makes delayed old-incarnation notifications harmless to
  successor claims. Mutation
  receipts rotate through a bounded FIFO window instead of permanently
  exhausting the guardian after 65,536 operations, while original spawn
  identities remain protected for each pane's lifetime. Inputs awaiting a
  durable or terminal disposition are pinned against unrelated receipt
  pressure, and acknowledgement updates their retained request aliases through
  a reverse index instead of a fleet-wide scan. A per-effect alias ceiling
  prevents one ambiguous input from monopolizing the guardian's global receipt
  budget while preserving already admitted reconciliation identities. Receipt
  rotation evicts an effect and every surviving request alias as one coherent
  identity unit, so a newer alias cannot outlive its effect fingerprint and
  later disagree with a reused bounded-window effect UUID. Runtime durability
  completion is likewise bound to the full authenticated pane/mux/generation/
  sequence/effect/payload-digest fingerprint, so a delayed acknowledgement for
  an evicted receipt cannot complete a newer input that reused its UUID. Input
  effect queries now carry the original input sequence and payload digest in a
  fixed tagged payload; UUID reuse with a different fingerprint fails closed
  rather than returning the newer input's disposition. A rotated receipt below
  the pane's consumed sequence fence reports `disposition_unavailable` instead
  of the resend-authorizing `not_seen`, and that uncertainty state is rejected
  from newly accepted input acknowledgements. Runtime
  effects use a separate transactional API: authentication, generation,
  sequence, capacity, and idempotency checks precede the callback; a failed
  zero-effect callback neither publishes a spawn, advances its lease, nor
  evicts any historical replay receipt; and
  exact replays never invoke the callback again. Ambiguous input is explicitly
  committed as accepted-not-durable for exact reconciliation, never exposed as
  a safely retryable failure. Spawn requests carry a bounded serialized
  `CommandBuilder` plus fixed PTY geometry and require its one canonical v1
  byte encoding, so ignored JSON fields cannot become an authenticated hidden
  payload. Default diagnostics now omit content-derived payload digests from
  request/response headers, input-effect queries, durability identities, and
  retained state; hiding only the raw input would leave low-entropy commands
  dictionary-testable. Resize and signal requests use fixed operation-tagged payloads, and
  every currently payload-free control
  operation—including checkpoint and replay—rejects hidden trailing bytes.
  Success replies likewise use operation-typed fixed
  binary payloads, and the public envelope can only be created through the
  typed success/rejection constructors; the common encoder and decoder both
  revalidate operation scope, reply schema, and response identity. Bounded
  fixed-width census rows preserve the full `i32`
  exit-status range with an explicit presence bit, and the correlated client
  rechecks pane, effect, generation, sequence, census cursor, and the exact
  requesting page's entry and encoded-byte ceilings inside the authenticated
  payload before consuming it. The server-side success constructor applies
  those same per-request page ceilings, so it cannot authenticate an oversized
  page merely because the page remains under the protocol-global cap. These are
  protocol/state proofs only; they do not yet claim live PTY continuity.
  Non-success responses now use fixed content-free rejection codes whose
  transient/terminal classification is authenticated and checked before the
  correlated client exposes it; arbitrary runtime error strings never become
  guardian control-plane payloads.
- Routes authenticated CheckpointStage, Checkpoint, Replay, and ReplayAck
  operations through the guardian's fixed-capacity off-readiness worker, then
  restores the sole protocol authority before routing each generation-fenced
  completion. A staged mux-side guardian proxy now claims one durable pane,
  replays the newest compatible canonical terminal checkpoint plus its exact
  raw-output suffix into inert handlers, verifies every cursor, record, digest,
  geometry, and completion boundary, and only then publishes a live pane.
  Lost Replay and ReplayAck replies retain their exact request identities for
  bounded retry, and any unpublished staged or activated claim is retired by
  an exact identity-bound rollback guard. Tail reads remain adaptively polled
  while server-side `wait_millis` readiness deferral is completed; production
  guardian activation remains disabled.

### Known continuity boundary

- Live replacement of panes created by the legacy `LocalPane` path is still
  intentionally unavailable because those mux processes own their PTY masters
  and child handles. The guardian lease/replay/checkpoint path applies only to
  panes born under guardian ownership, and its production selector remains
  disabled until broker restart recovery, lease rotation, replay readiness,
  durable topology reconciliation, and real `SIGKILL` upgrade proof are
  complete. Deployment fails closed rather than describing a disruptive
  restart or transcript export as lossless process continuity.

Representative landed commits:
[matched release process family](https://github.com/Dicklesworthstone/frankenterm/commit/80effb4302fef9f7cc172f6e3aff45f19870ab77),
[persistent domain-spawn diagnostics](https://github.com/Dicklesworthstone/frankenterm/commit/93556402bdde9c63a8be0dd98bd94b2f64597749),
[private dump and codec remediation](https://github.com/Dicklesworthstone/frankenterm/commit/569bfa89dd7ecfe2bc2845f845d65e09a5e34299),
[durable pane and scrollback authority](https://github.com/Dicklesworthstone/frankenterm/commit/2342cc309e5a2b2e9f6532f390ca1be3e0c30530), and
[operator contract updates](https://github.com/Dicklesworthstone/frankenterm/commit/9048d163019dc51c5f95e76fe2d3da8cf59b873a).

---

## [0.15.1] -- 2026-08-21 (GitHub Release)

GitHub Release: <https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.15.1>
Compare: <https://github.com/Dicklesworthstone/frankenterm/compare/v0.15.0...v0.15.1>

### Release-contract repair

- Restores the full native release process after `v0.15.0` was published without binary assets.
- Ships verified CLI archives for Linux x86_64, Linux arm64, macOS arm64, and Windows x86_64, plus the complete macOS `FrankenTerm.app` bundle.
- Restores formatter-clean source, per-asset SHA-256 sidecars, aggregate `SHA256SUMS`, local macOS GUI installation, LaunchServices registration, and Dock refresh.
- Seals every macOS bundle executable with the exact `release-interactive` profile identity instead of Cargo's generic inherited `release` label, so mixed-profile bundles fail closed before publication.
- Preserves quiet exit `141` for real closed-pipe writes from release binaries built with compiler-remapped standard-library source paths, while continuing to reject caller-forged EPIPE panic payloads.
- Correctly terminates Unix subprocess groups on cancellation by separating the negative process-group identifier from `kill` option parsing, with an exact live process-group regression.
- Makes timeout arbitration return an already-ready result before installing an elapsed timer, and preserves completion signalling after blocking work observes cancellation.
- Continues bounded FIFO size eviction across SQLite's quantized page-release boundary while reporting an unattainable cap honestly when only the schema floor remains.
- Restores clean-host release reproducibility by pinning `rich_rust` to its reachable 0.2.3 revision, and fixes Windows watch-claim delivery to own its asynchronous line payload.

---

## [0.15.0] -- 2026-08-20 (GitHub Release)

GitHub Release: <https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.15.0>
Compare: <https://github.com/Dicklesworthstone/frankenterm/compare/v0.14.1...v0.15.0>

This release was published with source archives only and no installable binary assets. It is superseded by `v0.15.1`.

### Sampled paste tracing

- Adds additive codec v63 PDU99 `SendPasteTracedV1` while preserving byte-identical PDU13 paste traffic for older supported peers.
- Carries content-free sampled trace authority from the client to server K4/K5 stages without copying paste content into trace events.
- Validates traced-paste path, serial, and payload bounds before framing, and retains the ordinary paste path below negotiated codec v63.

---

## [0.14.1] -- 2026-08-20 (GitHub Release)

GitHub Release: <https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.14.1>
Compare: <https://github.com/Dicklesworthstone/frankenterm/compare/v0.14.0...v0.14.1>

### Security and release correctness

- Pane input now stays on the direct mux transport. If that transport is unavailable, the operation fails closed instead of exposing arbitrary terminal input in subprocess arguments.
- Bridge I/O failures now return finite diagnostics that cannot echo backend or credential-bearing error details.
- Signed historical product-catalog tests no longer depend on mutable ambient Beads or README state, and the workspace-wide Rust formatting gate is restored.

---

## [0.14.0] -- 2026-08-20 (GitHub Release)

GitHub Release: <https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.14.0>
Compare: <https://github.com/Dicklesworthstone/frankenterm/compare/v0.13.0...v0.14.0>

1,313 non-merge commits after the v0.13.0 tag. This is a navigation aid, not an exhaustive dump: the dominant landed work is mux exact-owner/census/codec authority, scheduler admission, recorder truth and delivery durability, plus the 2026-08-19 janitor docs-reorg.

### Delivered capability

- Mux exact-owner/census/codec authority on post-0.13 `main`.
- Root ELF/scratch removed; planning/wizard cluster now under `docs/planning/`.

### Closed workstreams

- Tracker: [`.beads/issues.jsonl`](https://github.com/Dicklesworthstone/frankenterm/blob/main/.beads/issues.jsonl).

### Janitor docs-reorg (2026-08-19)

Root ELF/scratch and the planning/wizard cluster left the repository root and now live under `docs/`.

- Removed root `pat` ELF, `output.txt`, `HELLO_AGENTS.txt`, and `claude-upgrade-progress.json`; archived one-shot bead scripts under `scripts/archive/`.
- Relocated `PLAN.md`, `PLAN_CODEX.md`, `UPGRADE_LOG.md`, `UPGRADE_TODO.md`, `AGENT_TODO.md`, the `PLAN_TO_DEEPLY_INTEGRATE_*` series, and the full wizard-report cluster into [`docs/planning/`](https://github.com/Dicklesworthstone/frankenterm/tree/main/docs/planning) and [`docs/planning/wizards/`](https://github.com/Dicklesworthstone/frankenterm/tree/main/docs/planning/wizards).

### Representative commits

- [`bb6809b3d7ab4cda8d4f264a01718003618d8ec9`](https://github.com/Dicklesworthstone/frankenterm/commit/bb6809b3d7ab4cda8d4f264a01718003618d8ec9) — `chore(janitor): remove root ELF/scratch; move plans and wizard cluster into docs/`.

### Mux exact-owner authority, census, and codec

The post-0.13 mux campaign binds tabs and pane trees to exact owner generations, charges snapshot work against a request-scoped census ledger, and shrinks hot PDU frames.

- Transport-independent `MuxOperation` identity on every mux call ([`60062f0fb0ce5717c97f7a798cb920573ed801b8`](https://github.com/Dicklesworthstone/frankenterm/commit/60062f0fb0ce5717c97f7a798cb920573ed801b8)).
- Request-scoped pane-census ledger with exact-boundary proofs ([`33288f766d17a0c655aa59b7e0eb3d4766a3e7e3`](https://github.com/Dicklesworthstone/frankenterm/commit/33288f766d17a0c655aa59b7e0eb3d4766a3e7e3)); complete census work telemetry ([`cad84ed15f411a56364ee28af7701fcb99af56ec`](https://github.com/Dicklesworthstone/frankenterm/commit/cad84ed15f411a56364ee28af7701fcb99af56ec)); yield during large pane snapshots ([`fe0c098337d5c6c1cea093d54e2546de0181a1e0`](https://github.com/Dicklesworthstone/frankenterm/commit/fe0c098337d5c6c1cea093d54e2546de0181a1e0)).
- Structural-owner index (tiled vs floating) and prepared pane-tree binding ([`5655efa090c35131dac0ab54d2391886b0dd23b0`](https://github.com/Dicklesworthstone/frankenterm/commit/5655efa090c35131dac0ab54d2391886b0dd23b0), [`d33b0800d2a21cd6c3f8cb151948a07051e0c9c6`](https://github.com/Dicklesworthstone/frankenterm/commit/d33b0800d2a21cd6c3f8cb151948a07051e0c9c6)); exact in-place tab-generation replace and move-by-registration ([`f28a8cc64ea9fd3f93f03f1d0e0ef971ae2a3a24`](https://github.com/Dicklesworthstone/frankenterm/commit/f28a8cc64ea9fd3f93f03f1d0e0ef971ae2a3a24)).
- Hot mux PDU frame shrink ([`b7d0821c5b58c657e6cf63756a9a9c8048a8919d`](https://github.com/Dicklesworthstone/frankenterm/commit/b7d0821c5b58c657e6cf63756a9a9c8048a8919d)); bounded GUI scheduler lanes ([`09b3c4b1a4f60a19ead56f7f1c1d0fff03305587`](https://github.com/Dicklesworthstone/frankenterm/commit/09b3c4b1a4f60a19ead56f7f1c1d0fff03305587)).

### Runtime / asupersync upgrade path

- Asupersync pair advanced to 0.3.10 ([`1b5f2519457ef0a94d5a55a1903cb6cfe7f0581d`](https://github.com/Dicklesworthstone/frankenterm/commit/1b5f2519457ef0a94d5a55a1903cb6cfe7f0581d)); mixed-runtime codec prefers asupersync ([`5e04c80eac2217b7bf8ec31bcad8621a78559777`](https://github.com/Dicklesworthstone/frankenterm/commit/5e04c80eac2217b7bf8ec31bcad8621a78559777)).
- Upgrade re-audit of the Asupersync 0.4.x gate against 0.4.8 and FastMCP 0.6.0 ([`c87a6962316aaa213a2b9c5ea6f209b8ea15b5e2`](https://github.com/Dicklesworthstone/frankenterm/commit/c87a6962316aaa213a2b9c5ea6f209b8ea15b5e2)); asupersync/fsqlite target refresh ([`c70011ae95def8131fdd02929a63fc8fedeb16a0`](https://github.com/Dicklesworthstone/frankenterm/commit/c70011ae95def8131fdd02929a63fc8fedeb16a0)).

---

## [0.13.0] -- 2026-07-28 (GitHub Release)

GitHub Release: <https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.13.0>
Published 2026-07-28 (`v0.13.0 — test-suite honesty campaign + tx/capture/redaction hardening; full platform matrix returns`).
Tag `v0.13.0` created 2026-07-27 at [`c366f3ac95a2a53d6d86e438f1432bdcf4981f26`](https://github.com/Dicklesworthstone/frankenterm/commit/c366f3ac95a2a53d6d86e438f1432bdcf4981f26).
Compare: <https://github.com/Dicklesworthstone/frankenterm/compare/v0.12.0...v0.13.0>

128 commits since v0.12.0. Two campaigns: make the unfiltered `frankenterm-core --lib` suite honest (ft-nam3s — 29,600+ tests, 85 known failures → zero deterministic failures), and harden the tx/capture/redaction production paths those recovered tests exposed. Restores the full binary matrix: Linux (x86_64 + arm64), macOS (Apple Silicon CLI + FrankenTerm.app), and Windows (x86_64).

### Test-suite honesty (ft-nam3s)

The unfiltered `frankenterm-core --lib` suite went from never-terminating, to 85 known failures, to zero. Process-global counters/config/fault-injection became test-scoped; never-executed tests that had rotted into contradiction with production were rebuilt on their preserved intent; nine of the failures were real production bugs (fixed below).

- Residual `--lib` failures cleared across 21 modules ([`cb70b68750885757ddfabf623f68c60dd2b69e9e`](https://github.com/Dicklesworthstone/frankenterm/commit/cb70b68750885757ddfabf623f68c60dd2b69e9e)).
- Zero-failure ratchet: tx lease grace, MCP counter locks, FTS error mapping ([`a0e13e828fe5c3b69c4497c598ac72b838462a8d`](https://github.com/Dicklesworthstone/frankenterm/commit/a0e13e828fe5c3b69c4497c598ac72b838462a8d)).
- Version bump + bounded `ETXTBSY` spawn retry in `SubprocessBridge` ([`7362cbb5ce34a6f46c30600d0456e1f764e2ea91`](https://github.com/Dicklesworthstone/frankenterm/commit/7362cbb5ce34a6f46c30600d0456e1f764e2ea91)).

### Production bugs the recovered suite found

- **Capture integrity** — overlap windows size from the snapshot, not a fixed budget ([`de562f2b8e5ef007c7689c172214cd3a7e6d064f`](https://github.com/Dicklesworthstone/frankenterm/commit/de562f2b8e5ef007c7689c172214cd3a7e6d064f)); coincidental/boundary-noise overlaps refused and resumed capture anchored ([`549754cd4de39916f370bc41970a073c6a29763b`](https://github.com/Dicklesworthstone/frankenterm/commit/549754cd4de39916f370bc41970a073c6a29763b)).
- **Async runtime** — wrapper-built runtimes get a real blocking pool so `spawn_blocking` no longer runs inline and freezes timers/cancel watchers ([`f223887addbc90c47cd979e8535e827be4feb808`](https://github.com/Dicklesworthstone/frankenterm/commit/f223887addbc90c47cd979e8535e827be4feb808)).
- **Tx engine** — contract store durable, TOCTOU-safe, and wired into production ([`37fb43d0ec74486a364af08f34cff3948926affd`](https://github.com/Dicklesworthstone/frankenterm/commit/37fb43d0ec74486a364af08f34cff3948926affd)); rollback compensation requires authoritative durable proof taken atomically before mutation ([`a5ad34ab55c8d46143b9c5c70dd3fefdcbe0d2f9`](https://github.com/Dicklesworthstone/frankenterm/commit/a5ad34ab55c8d46143b9c5c70dd3fefdcbe0d2f9)); storeless execution entrypoints sealed against effectful executors ([`e1508558cb8fc6248e458ac688f529de30439778`](https://github.com/Dicklesworthstone/frankenterm/commit/e1508558cb8fc6248e458ac688f529de30439778)).
- **Redaction** — four bypasses closed ([`6f27516bc407635ced44cfcc52200525bce2ad12`](https://github.com/Dicklesworthstone/frankenterm/commit/6f27516bc407635ced44cfcc52200525bce2ad12)); streaming retention bounded so ordinary output drains ([`08e98414d218e4531248f54fa73c942147673ad3`](https://github.com/Dicklesworthstone/frankenterm/commit/08e98414d218e4531248f54fa73c942147673ad3)); streaming emit boundary made linear ([`44bfeba105fc3b09545175a9ee084fde1d1763d5`](https://github.com/Dicklesworthstone/frankenterm/commit/44bfeba105fc3b09545175a9ee084fde1d1763d5)).
- **Policy & command guard** — `[safety].block_alt_screen` approval gate reachable ([`e46f9d1c62b9cc5447be333b82754e3a20ec22c0`](https://github.com/Dicklesworthstone/frankenterm/commit/e46f9d1c62b9cc5447be333b82754e3a20ec22c0)); rm/git/chmod rules attribute by command position so `aws s3 rm` / `docker rm` do not inherit `core.filesystem:rm-rf` ([`ecb6a820ded73da3e24ffc18c78f6812f44011eb`](https://github.com/Dicklesworthstone/frankenterm/commit/ecb6a820ded73da3e24ffc18c78f6812f44011eb)).
- **Storage** — size-cap eviction deletes proportionally instead of wiping every segment ([`88e68c88a6c92fff31a78f72f70a73bc7cd52ed8`](https://github.com/Dicklesworthstone/frankenterm/commit/88e68c88a6c92fff31a78f72f70a73bc7cd52ed8)); single-append path writes semantic embeddings ([`8056b1f4e8627fa2da3c5d65650270cc6b00cbfe`](https://github.com/Dicklesworthstone/frankenterm/commit/8056b1f4e8627fa2da3c5d65650270cc6b00cbfe)); orphan pane-state cleanup counts correctly under either FK setting ([`daec3eca46cbbeafa43580088a17fc354fb2183d`](https://github.com/Dicklesworthstone/frankenterm/commit/daec3eca46cbbeafa43580088a17fc354fb2183d)).
- **Mux / MCP** — Progress alerts no longer deduped-then-forwarded twice ([`524d1e76e44167bd39440ab56e8d0d3556f451e3`](https://github.com/Dicklesworthstone/frankenterm/commit/524d1e76e44167bd39440ab56e8d0d3556f451e3)); tx contract errors moved onto the FT-MCP taxonomy ([`9f6c5e1ce3a225a4d5e8f5c308eaa7d7c5eaa794`](https://github.com/Dicklesworthstone/frankenterm/commit/9f6c5e1ce3a225a4d5e8f5c308eaa7d7c5eaa794)).

### Features

- Robot connector lifecycle family: handler, persistence, and contract ([`4a6b9b22037c76e60e250ef6e7af55633ca2760e`](https://github.com/Dicklesworthstone/frankenterm/commit/4a6b9b22037c76e60e250ef6e7af55633ca2760e)).
- `robot.tx_rollback_proof_missing` / `_conflict` published on the robot error surface ([`1282b15011fcf5d21a6a70ddfc1faf5744139184`](https://github.com/Dicklesworthstone/frankenterm/commit/1282b15011fcf5d21a6a70ddfc1faf5744139184)).
- Product journey catalog v1 — fail-closed product truth contract ([`32d72991856a9b00d55086ca07384dc082b8a3fc`](https://github.com/Dicklesworthstone/frankenterm/commit/32d72991856a9b00d55086ca07384dc082b8a3fc)).
- Mux-server render-delivery ledger and scheduler contracts frozen ([`5c12432bb10eb989e223122365344609d1d457bf`](https://github.com/Dicklesworthstone/frankenterm/commit/5c12432bb10eb989e223122365344609d1d457bf)).

---

## [0.12.0] -- 2026-06-29 (GitHub Release)

GitHub Release: <https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.12.0>

Runtime churn fix (asupersync 0.3.5) + window-state persistence.

- **Thread-per-sleep churn eliminated** — bumped the asupersync runtime to 0.3.5,
  which replaces the OS-thread-per-timer fallback (a `pthread_create`/`exit` per
  `time::sleep`) with a single shared process-global fallback timer. On a settled
  mux session this cut Sleep-poll thread spawns from ~1591/20s to **0** and dropped
  idle CPU's upper range from ~65% to ~39%. (The residual idle CPU is a separate
  `sched_yield` busy-spin lever, tracked upstream in asupersync.)
- **Window maximize/fullscreen persistence** — windows reopen maximized or
  fullscreen if they were when you quit, remembered per workspace
  (`DATA_DIR/window-state.json`). A missing/corrupt file falls back to today's
  default geometry; nothing changes for un-maximized windows.
- **Own-lib refresh** — asupersync, asupersync-macros, rich_rust, fastmcp_rust,
  fastapi_rust, frankensearch bumped to their latest main; the ecosystem now
  unifies on a single asupersync 0.3.5 (fastapi_rust's exact `=0.3.4` pin relaxed
  upstream). Ported the optional `distributed` feature to asupersync 0.3.5's
  `http::h1` client API.

---

## [0.11.0] -- 2026-06-27

Mux latency-hiding + mosh-grade predictive echo for remote (SSH-mux) panes.

- **Non-blocking remote-pane writer** — typing into a slow or dead/reconnecting
  SSH-mux pane no longer freezes the whole GUI (the writer previously drove a
  blocking, un-timed RPC on the GUI main thread).
- **Zero-poll liveness** — the per-pane liveness poll collapses to a slow backstop
  (disconnect is detected by the transport reader + `PaneRemoved` push, both
  poll-independent), cutting redundant round-trips on active panes.
- **Viewport prefetch** — speculative ±one-viewport read-ahead so scrolling a
  high-latency remote pane doesn't stall a round-trip per page.
- **Predictive local echo (mosh-grade)** — typing into a moderate-latency remote
  pane (~25 ms+) echoes instantly; predictions are validated against the server
  with a per-pane confidence model, rendered glitchlessly once confident, and
  suppressed in echo-off prompts (password/passphrase) and full-screen TUIs
  (alt-screen). Default `local_echo_threshold_ms` lowered 100 → 20 ms.

(The earlier 0.10.x optimization rounds are recorded in the git history rather
than here.)

## [0.1.0] -- 2026-04-11

First feature-complete changelog baseline. Establishes what works, what is feature-gated, and what is in progress.

### Working (passes end-to-end)

- **Observation pipeline** — `ft watch` discovers panes via WezTerm CLI polling or native push events, populates SQLite pane table
- **Pattern detection** — Aho-Corasick multi-pattern scanner with configurable triggers, BOCPD change-point detection
- **Robot Mode** — `ft robot state`, `ft robot send-text`, `ft robot wait-for` for machine-driven pane interaction
- **Full-text search** — FrankenSearch hybrid (BM25 + semantic) with RRF fusion, tantivy indexing
- **Policy engine** — rule-based allow/deny with actor, surface, action, and pane-id matching
- **Fleet memory controller** — 4-tier pressure management (Normal/Elevated/Critical/Emergency) with tiered scrollback eviction
- **Session persistence** — SQLite-backed workspace state with atomic snapshot/restore
- **Connector SDK** — inbound/outbound bridge, mesh routing, capability envelopes
- **Transactional execution** — `ft tx run` with prepare gates (policy, liveness, reservation, approval), commit/compensate phases, audit trail
- **Diagnostic subsystem** — `ft doctor --json` with 31 health checks

### Feature-gated (compile-time opt-in)

- **MCP** (`--features mcp`) — Model Context Protocol tool surface with bridge/middleware/proxy
- **Distributed mode** (`--features distributed`) — mTLS node-to-node communication scaffolding
- **Semantic search** (`--features semantic-search`) — fastembed v5.11 vector embeddings
- **Web API** (`--features web`) — FastAPI-based HTTP server for `/health`, `/panes`, `/events`, `/search`
- **TUI** (`--features tui`) — FrankenTUI operator dashboard (in development)

### Partial (framework complete, data plane in progress)

- **Mission dispatch** — planner, event log, lifecycle state machine implemented; live agent coordination pending
- **Native mux server** — lifecycle engine, topology orchestration, command transport; vendored WezTerm backend still primary

### Known limitations

- **Build requirements** — Rust nightly, ~4 GB RAM, ~30 GB disk for full build; macOS needs `CC`/`CXX` set to avoid shell alias conflicts
- **WezTerm dependency** — core observation loop requires WezTerm mux server as backend
- **Codebase scale** — ~793k LOC across 120 crates; cold build takes 15-30 minutes

### Installation

```sh
cargo install --git https://github.com/Dicklesworthstone/frankenterm.git --bin ft frankenterm
```

On macOS, if `cc` is aliased to something other than the C compiler:
```sh
CC=$(xcrun --find clang) CXX=$(xcrun --find clang++) cargo install ...
```

---

## [0.1.0+] -- development on `main` after the 0.1.0 baseline (through 2026-05-18)

> Historical notes written while this window was still unreleased. The work below landed on `main` between the 0.1.0 changelog baseline and the later tagged 0.2.0–0.12.0 releases; it is **not** the current Unreleased window (that is v0.13.0..HEAD at the top of this file). Roughly 3,550 commits land on `origin/main` between 2026-05-01 and 2026-05-18 across the concurrent agent swarm, with 3,558 commits in the same window on local `main` as of that snapshot.

### RCH pressure, Agent Mail failover, and static proof hardening (2026-05-18)

The newest swarm-operations work turns recurring pressure incidents into explicit, replayable contracts. RCH proof lanes now document why workers are rejected, Agent Mail outages have a no-service fallback path, and the static/golden fixture corpus keeps expanding around proof artifacts that used to live only in operator memory.

- **Worker-storage recovery contracts** — inventory, approval, recovery-proof gate, and operator runbook coverage landed for the RCH worker-storage path ([`0ac9e6260`](https://github.com/Dicklesworthstone/frankenterm/commit/0ac9e6260), [`2f0ebc284`](https://github.com/Dicklesworthstone/frankenterm/commit/2f0ebc284), [`22f9b4ae5`](https://github.com/Dicklesworthstone/frankenterm/commit/22f9b4ae5), [`6a54ffa24`](https://github.com/Dicklesworthstone/frankenterm/commit/6a54ffa24)); the recovery proof command was then canonicalized to the remote-only RCH lane ([`0a8fd958f`](https://github.com/Dicklesworthstone/frankenterm/commit/0a8fd958f)). A follow-up command fixture fix exists locally as `0f60d92d8` and is intentionally unlinked until pushed.
- **Agent Mail failover became a tested path** — the failover plan now has a snapshot contract, retry classifiers, no-service gate, stale-reopen policy, runbook, completion audit, and retained verifier coverage ([`b2503e532`](https://github.com/Dicklesworthstone/frankenterm/commit/b2503e532), [`eefe81974`](https://github.com/Dicklesworthstone/frankenterm/commit/eefe81974), [`c3dd5fd3c`](https://github.com/Dicklesworthstone/frankenterm/commit/c3dd5fd3c), [`a279a0de3`](https://github.com/Dicklesworthstone/frankenterm/commit/a279a0de3), [`42446024e`](https://github.com/Dicklesworthstone/frankenterm/commit/42446024e), [`b96809e31`](https://github.com/Dicklesworthstone/frankenterm/commit/b96809e31), [`77b744062`](https://github.com/Dicklesworthstone/frankenterm/commit/77b744062)).
- **Static/golden proof corpus widened** — negative fixtures and artifact-path hardening now cover operating-envelope, mission-planner, RCH worker inventory/approval, task-fit passports, provider quotas, capacity signals, resource cockpit, and disk-guard proof artifacts ([`2c0149e18`](https://github.com/Dicklesworthstone/frankenterm/commit/2c0149e18), [`6600907ab`](https://github.com/Dicklesworthstone/frankenterm/commit/6600907ab), [`18acdbf63`](https://github.com/Dicklesworthstone/frankenterm/commit/18acdbf63), [`70f9784f8`](https://github.com/Dicklesworthstone/frankenterm/commit/70f9784f8), [`f21d3e897`](https://github.com/Dicklesworthstone/frankenterm/commit/f21d3e897)). The local `56382ae10` verifier-command pin is not linked until it is pushed.
- **Poison-lock and edge-case correctness sweep** — recovery/guard fixes landed across Wayland, X11, scripting, replay, storage, mux, windowing, PTY, font, ARS, promise, event-stream, IO, resize, WASM, audit, and core surfaces; representative correctness fixes include sender-callback lock release, sparse logical-line lookup guards, image byte-count hardening, clipboard pipe IO hardening, and RCH admission scope filtering ([`e2751a849`](https://github.com/Dicklesworthstone/frankenterm/commit/e2751a849), [`ff41a0717`](https://github.com/Dicklesworthstone/frankenterm/commit/ff41a0717), [`e17d09d4f`](https://github.com/Dicklesworthstone/frankenterm/commit/e17d09d4f), [`ddff66f45`](https://github.com/Dicklesworthstone/frankenterm/commit/ddff66f45), [`21a40b110`](https://github.com/Dicklesworthstone/frankenterm/commit/21a40b110)). Local setup-marker validation exists as `d3f4abd94` pending RCH proof/push coordination.
- **Proof-policy and count refreshes** — docs now require remote-only RCH proof commands, while README/count placeholders and agent-count attestations were restamped from live repository measurements ([`d1db65e42`](https://github.com/Dicklesworthstone/frankenterm/commit/d1db65e42), [`2a9baee39`](https://github.com/Dicklesworthstone/frankenterm/commit/2a9baee39), [`1f2c52002`](https://github.com/Dicklesworthstone/frankenterm/commit/1f2c52002), [`de8b84395`](https://github.com/Dicklesworthstone/frankenterm/commit/de8b84395)). Local-only follow-ups currently cover retained proof commands, NTM proof examples, operator proof examples, RCH admission no-service fixture commands, and README/AGENTS proof examples (`474e7d4c`, `d1938a57a`, `cac9dddb0`, `c803631f`, `48cfde4e`) pending push coordination.

### Operating envelope + incident-bundle plumbing (2026-05-10 -- 2026-05-16)

The swarm-safety stack matures from "we have telemetry" to "we fail closed when telemetry is missing."

- **Operating-envelope contract shipped** (ft-booek + ft-booek.1) — `ft.operating_envelope.v1` contract with a side-effect-free planner module and golden fixtures lands as the canonical capacity-admission surface ([`b5bd5d352`](https://github.com/Dicklesworthstone/frankenterm/commit/b5bd5d352)).
- **Operator runbook gate** (ft-booek.6) — adds `docs/operator-runbook.md` gate entry that operators reach from `ft doctor` when envelope state is degraded ([`d71b8076b`](https://github.com/Dicklesworthstone/frankenterm/commit/d71b8076b)).
- **Fail-closed posture for missing telemetry** — telemetry/network-pressure paths now refuse to advance when their measurement source is absent, instead of papering over with defaults:
  - missing process snapshots fail closed in `telemetry` ([`fe8a5bb6a`](https://github.com/Dicklesworthstone/frankenterm/commit/fe8a5bb6a))
  - `core` fails closed on missing network-pressure telemetry (ft-9wp2u, [`1f596e721`](https://github.com/Dicklesworthstone/frankenterm/commit/1f596e721))
  - `operating-envelope` fails closed on RCH critical pressure ([`8ec3c5db3`](https://github.com/Dicklesworthstone/frankenterm/commit/8ec3c5db3))
- **Crash + incident-bundle swarm sources** (ft-9sy9e family) — wires swarm incident-bundle sources to live collectors ([`fc6dfc97e`](https://github.com/Dicklesworthstone/frankenterm/commit/fc6dfc97e)) and adds a publish-side snapshot path so producers don't have to re-derive bundle inputs ([`bd70318ff`](https://github.com/Dicklesworthstone/frankenterm/commit/bd70318ff)).
- **Beads coordination snapshot** (ft-tkkqx) — collector against `.beads/issues.jsonl` so incident bundles carry the live task-tracker state ([`c6e63354e`](https://github.com/Dicklesworthstone/frankenterm/commit/c6e63354e)).
- **Mission objective planner** (ft-auy2g, capacity-aware) — core planner + source adapters + golden corpus ([`824fc50dd`](https://github.com/Dicklesworthstone/frankenterm/commit/824fc50dd), [`d312ecf0d`](https://github.com/Dicklesworthstone/frankenterm/commit/d312ecf0d), [`e94d0885c`](https://github.com/Dicklesworthstone/frankenterm/commit/e94d0885c)).

### Reality-check round 2 substrate (2026-05-12, ft-tf6g3)

The second `/reality-check-for-project` epic opens with a wide substrate pass that names the remaining outward-facing gaps and stands up the machinery to close them.

- **Final-mile convergence epic** (ft-tf6g3) — umbrella for the second reality-check run; closes the attestation graph, the headline-claim artifact links, the renderer SLO suite, and the round-3 statistical elevations the bridge plan owes (Lindley/min-plus, Fano, SPRT, conformal bands, Mazurkiewicz cancel-traces, TLA+ tx-killswitch, Stateright work-family atomicity).
- **Renderer SLO catalog** — consolidated `docs/perf/resize-quality-slo.md` + machine-readable JSON; scheduler/reflow stage budgets land at `docs/resize-performance-slos.md`.
- **`ft-perf-gate` substrate crate** — SPRT + conformal + KL-divergence + causal-DAG primitives extracted to their own workspace crate so attestation gating can compose them without leaking through `frankenterm-core`.
- **`ft-test-log` substrate** — centralized test-logging convention crate for the evidence-stream fixture corpus.
- **Resource-pressure cockpit contract** — `docs/resource-pressure-cockpit-contract.md` separates `rust_heap`, `mmap_file_backed`, `sqlite_page_cache`, `graphics_media`, `scrollback_cache`, `child_processes`, and `unknown` residency before anything is called a leak.
- **Target-class hardware gate** — `docs/perf/target-class-hardware.md` defines the per-SKU artifact contract; the current `linux-x86_64-high-core` artifact stays `skipped_not_proven` and 200-pane / high-scale memory wording is held back accordingly.

### Substrate audit waves (2026-04-15 -- 2026-05-02)

Multi-pass substrate audit closes eight defect families across the codebase. Each fix is small; the discipline is that the families were systematically swept rather than spot-patched.

- **Public-field-bypass family** — eight audit-fix commits closing nine pub-field bypass sites where `pub` struct fields let callers bypass clamping, validation, or invariants. Fixed across `audit_erasure_spec::ErasureShard` ([`7cfb6218f`](https://github.com/Dicklesworthstone/frankenterm/commit/7cfb6218f)) and `ErasureConfig` ([`f47a2451e`](https://github.com/Dicklesworthstone/frankenterm/commit/f47a2451e)), `circuit_breaker::Config` ([`29c028e01`](https://github.com/Dicklesworthstone/frankenterm/commit/29c028e01)), `latency_model::QuantileBudgetMs` ([`b4bb7c373`](https://github.com/Dicklesworthstone/frankenterm/commit/b4bb7c373)) and `network_calculus_bound::{ArrivalCurve, ServiceCurve}` ([`b79f814a8`](https://github.com/Dicklesworthstone/frankenterm/commit/b79f814a8)), `approval::ApprovalScope`/`AuditContext` ([`611e0573e`](https://github.com/Dicklesworthstone/frankenterm/commit/611e0573e)), `subpixel_positioning::ScaleFactor` ([`46c8f8af9`](https://github.com/Dicklesworthstone/frankenterm/commit/46c8f8af9)), `font_features::AxisValue` ([`a45045e39`](https://github.com/Dicklesworthstone/frankenterm/commit/a45045e39)).
- **Rubber-stamp `is_safe` family** — seventeen audits where `is_safe()` returned true on cold-start, before measurements were recorded, or after a pure-rejection storm. Fixed across `display_pipeline_ci_matrix` (CRITICAL release-gate forgery, [`5b743bc6f`](https://github.com/Dicklesworthstone/frankenterm/commit/5b743bc6f)), `gpu_regression_fuzz_report` ([`35d52b848`](https://github.com/Dicklesworthstone/frankenterm/commit/35d52b848)), `redactor_coverage_matrix` ([`fd1d0bb23`](https://github.com/Dicklesworthstone/frankenterm/commit/fd1d0bb23)), `iterm2_osc_1337` ([`9a19c5b32`](https://github.com/Dicklesworthstone/frankenterm/commit/9a19c5b32)), `chaos::ChaosReport` ([`a399f598d`](https://github.com/Dicklesworthstone/frankenterm/commit/a399f598d)), `disaster_recovery_drills::ContinuityReport` ([`399f8157b`](https://github.com/Dicklesworthstone/frankenterm/commit/399f8157b)), `cell_consistency_crc` ([`94031e411`](https://github.com/Dicklesworthstone/frankenterm/commit/94031e411)), 7 cold-start doctor snapshots ([`8256d7554`](https://github.com/Dicklesworthstone/frankenterm/commit/8256d7554)), and others.
- **Sanitization-gap family** — `restore_process` rejects DEL + C1 controls including CSI ([`de6e21a51`](https://github.com/Dicklesworthstone/frankenterm/commit/de6e21a51)), `kitty_graphics_alt_text` closes HIGH sanitization bypass ([`2ec72f165`](https://github.com/Dicklesworthstone/frankenterm/commit/2ec72f165)), `browser::sanitize_path_component` blocks bare `.` and `..` ([`74caa6354`](https://github.com/Dicklesworthstone/frankenterm/commit/74caa6354)), `cass` closes argv flag-injection vector ([`bd617a723`](https://github.com/Dicklesworthstone/frankenterm/commit/bd617a723)).
- **NaN / unbounded-input family** — `chaos` rejects NaN probability + unbounded delay DoS + `p=1.0` false-negative ([`d06ef64d1`](https://github.com/Dicklesworthstone/frankenterm/commit/d06ef64d1)), `recorder_replay::ReplayConfig` + `VirtualClock` reject NaN + `NEG_INFINITY` ([`9558a155b`](https://github.com/Dicklesworthstone/frankenterm/commit/9558a155b), [`0e174c634`](https://github.com/Dicklesworthstone/frankenterm/commit/0e174c634)), `bench_stats::empirical_bernstein_ci` rejects non-finite range ([`c00847bcd`](https://github.com/Dicklesworthstone/frankenterm/commit/c00847bcd)), `disk_pressure::EwmaEstimator` sanitizes NaN alpha ([`78a2adea3`](https://github.com/Dicklesworthstone/frankenterm/commit/78a2adea3)).
- **Privacy-bypass family** — `scrollback_cold_tier::ChunkMetadata.redaction` made private to prevent skipping redactor ([`f9cff43d8`](https://github.com/Dicklesworthstone/frankenterm/commit/f9cff43d8)); `incident_bundle::truncate_file_content` honors `max_bytes_per_file` ([`4de738d04`](https://github.com/Dicklesworthstone/frankenterm/commit/4de738d04)) and `truncate_excerpt` honors `max_output_excerpt_len` ([`ae731d77b`](https://github.com/Dicklesworthstone/frankenterm/commit/ae731d77b)).
- **Redactor coverage expansion** (ft-8nd26) — adds JWT, GitLab, Twilio, SendGrid, and Datadog token patterns to the secret redactor ([`79b0d5d12`](https://github.com/Dicklesworthstone/frankenterm/commit/79b0d5d12)).
- **State-machine + telemetry correctness** — `pane_groups` closes 3 state-machine defects ([`f03cd10d7`](https://github.com/Dicklesworthstone/frankenterm/commit/f03cd10d7)), `triple_buffer_watchdog` closes 4 audit findings ([`5c43e4fea`](https://github.com/Dicklesworthstone/frankenterm/commit/5c43e4fea)), `sync_output_watchdog` closes 3 ([`fb155bd46`](https://github.com/Dicklesworthstone/frankenterm/commit/fb155bd46)), `frame_budget::SustainedBurstHarness` closes 3 ([`d67c59707`](https://github.com/Dicklesworthstone/frankenterm/commit/d67c59707)).
- **Workflow lock manager telemetry** (ft-rai3h) — `LockManagerHealth` telemetry surface for the workflow lock manager ([`3173f9f71`](https://github.com/Dicklesworthstone/frankenterm/commit/3173f9f71)).

### GUI render-state + renderer correctness (2026-04-15 -- 2026-05-16)

The GUI moves from "compiles" to "live render-state plumbed through paint and reduce-motion gates."

- **Live render-state wiring** — terminal triple-buffer registry, quad allocation snapshot, frame-budget reduce-motion gate, and render placeholder replacements all wired through to the live paint path ([`9ecb1df1a`](https://github.com/Dicklesworthstone/frankenterm/commit/9ecb1df1a)).
- **SynchronizedOutput (BSU/ESU)** — mux notification plumbing for BSU/ESU sync-output, drain telemetry into GUI, ApiSurface coverage made dynamic, and Operator drain-cause pinned for soft-reset under BSU ([`5f087525a`](https://github.com/Dicklesworthstone/frankenterm/commit/5f087525a), [`53cde2b47`](https://github.com/Dicklesworthstone/frankenterm/commit/53cde2b47), [`83c932646`](https://github.com/Dicklesworthstone/frankenterm/commit/83c932646)).
- **Drag handling classified** (ft-spcu0) — "drag not implemented" catch-all replaced with classified per-mode handlers ([`1edf9998e`](https://github.com/Dicklesworthstone/frankenterm/commit/1edf9998e)).
- **Command palette uses domain labels** (ft-dkd26) — palette renders the user-facing label instead of raw domain IDs ([`b0da8840b`](https://github.com/Dicklesworthstone/frankenterm/commit/b0da8840b)).
- **Tab progress indicator** — indeterminate-mode rendering for tabs whose work has unknown completion ([`8da923e5d`](https://github.com/Dicklesworthstone/frankenterm/commit/8da923e5d)).
- **Layer placeholder replacement** (ft-1l5n2) — compositor's placeholder `DrawCmd` replaced with the real draw command ([`fa0e427ef`](https://github.com/Dicklesworthstone/frankenterm/commit/fa0e427ef)).
- **Scripting dimensions expose window state** (ft-zfcsc) — Lua/scripting `window:get_dimensions()` returns the live window state instead of stale config ([`ef175fc68`](https://github.com/Dicklesworthstone/frankenterm/commit/ef175fc68)).
- **Renderer correctness substrate** — `iter_dirty` render-pass gate behind phase flag ([`2fccd13d0`](https://github.com/Dicklesworthstone/frankenterm/commit/2fccd13d0)), per-platform display probe schema ([`75bac2270`](https://github.com/Dicklesworthstone/frankenterm/commit/75bac2270)), platform reduce-motion probe primitive ([`ab9100dc0`](https://github.com/Dicklesworthstone/frankenterm/commit/ab9100dc0)), kitty graphics alt-text attestation generator + release gate ([`d418cd68e`](https://github.com/Dicklesworthstone/frankenterm/commit/d418cd68e)), stateful BSU ring buffer ([`30da3a5c4`](https://github.com/Dicklesworthstone/frankenterm/commit/30da3a5c4)), dirty-line frame-end clear predicate ([`fd4e98cbc`](https://github.com/Dicklesworthstone/frankenterm/commit/fd4e98cbc)), Wayland direct-scanout policy ([`cd9d4ba47`](https://github.com/Dicklesworthstone/frankenterm/commit/cd9d4ba47)).
- **Terminal protocol** — DEC private mode restore implemented ([`19d289158`](https://github.com/Dicklesworthstone/frankenterm/commit/19d289158)) with cache-reset coverage ([`0332dca07`](https://github.com/Dicklesworthstone/frankenterm/commit/0332dca07)); termwiz preserves input on empty history search ([`d102d12bd`](https://github.com/Dicklesworthstone/frankenterm/commit/d102d12bd)).
- **OSC protocol omnibus** (ft-ncwh5, ft-uea9o) — 3 audit findings closed across `osc_protocol_omnibus`, plus OSC 52 docstring honesty alignment.

### Native asupersync cutover closed (2026-04-01 -- 2026-04-15, ft-xbnl0.2)

The dual-runtime era ends. The remaining tokio-shaped seams in core observation, workflow, and maintenance loops collapse onto `Cx`-first asupersync.

- **Cx-first structured-concurrency entry points** (ft-xbnl0.2.2) — landed across `session_retention` ([`94faa1660`](https://github.com/Dicklesworthstone/frankenterm/commit/94faa1660)), `caut` ([`151d689cc`](https://github.com/Dicklesworthstone/frankenterm/commit/151d689cc)), `retry` ([`f4fbb7d47`](https://github.com/Dicklesworthstone/frankenterm/commit/f4fbb7d47)), and `native_events` ([`bd0aa03d1`](https://github.com/Dicklesworthstone/frankenterm/commit/bd0aa03d1)).
- **`timeout_with_cx` promoted to public API** ([`e4d6d62a0`](https://github.com/Dicklesworthstone/frankenterm/commit/e4d6d62a0)).
- **`broadcast` + `oneshot` channels migrated** to asupersync wrappers ([`e4ecb4700`](https://github.com/Dicklesworthstone/frankenterm/commit/e4ecb4700), [`154267b28`](https://github.com/Dicklesworthstone/frankenterm/commit/154267b28)).
- **Workspace crate defaults flipped** from `async-io`/`smol` to `async-asupersync` ([`4db7f7a62`](https://github.com/Dicklesworthstone/frankenterm/commit/4db7f7a62), [`1761e2a2d`](https://github.com/Dicklesworthstone/frankenterm/commit/1761e2a2d), [`7e6c334ff`](https://github.com/Dicklesworthstone/frankenterm/commit/7e6c334ff)).
- **LabRuntime test substrate** — `LabRuntime` deterministic tests for `DirectMuxClient` (wa-p48pw, [`3efa3a39c`](https://github.com/Dicklesworthstone/frankenterm/commit/3efa3a39c)), LabRuntime port regression guard + time-dependent comparison bench (wa-22x4r, [`94677214c`](https://github.com/Dicklesworthstone/frankenterm/commit/94677214c)), LabRuntime observation-loop tests and criterion benches (wa-1m7nk, [`9a9ce8691`](https://github.com/Dicklesworthstone/frankenterm/commit/9a9ce8691)).
- **Supported-path truth sweep closed** (ft-xbnl0.3.6) — final sweep with Rust SDK narrowed as the fully-supported envelope ([`302fcbf8e`](https://github.com/Dicklesworthstone/frankenterm/commit/302fcbf8e), [`8697fe0dd`](https://github.com/Dicklesworthstone/frankenterm/commit/8697fe0dd)).
- **No-runtime-regression gate** (ft-xbnl0.2.6) — explicit gate so future imports of `tokio::*` fail the build ([`a72fb92ca`](https://github.com/Dicklesworthstone/frankenterm/commit/a72fb92ca)).
- **ft-xbnl0 epic closed** — the goal-line epic for native asupersync + zero fake capabilities + verifiable mission completion is recorded as done in the beads ledger.

### Sub-crate carving completed (2026-04-25 -- 2026-05-03, ft-y0loj.* + post-hdvvo)

Layering enforced through extraction rather than discipline alone. `frankenterm-core` now has 19 sibling sub-crates plus three new utility/topology crates.

- **Cluster extractions** — `frankenterm-core-ars` (ARS subsystem, ~14k LOC), `frankenterm-core-tantivy` (lexical search, ~16k LOC), `frankenterm-core-replay` (~25k LOC), `frankenterm-core-fleet` (partial, fleet dashboard), `frankenterm-core-connectors` (connector boundary), `frankenterm-core-mcp` (MCP type boundary).
- **Leaf type crates** — `*-resource-types`, `*-error-types`, `*-config-types`, `*-policy-types`, `*-replay-types`, `*-telemetry-types`, `*-cass-types`, `*-caut-types`, `*-connector-types`, `*-audit-types`, `*-atlas-pack-types`, `*-x11-resize-types`.
- **Test infrastructure crate** — `frankenterm-core-test-macros` exposes `#[lab_runtime_test]` and friends so the LabRuntime substrate isn't trapped inside `frankenterm-core`.
- **Topology + perf gating** — new `frankenterm-topo`, `ft-perf-gate`, and `ft-test-log` workspace crates land for cross-cutting concerns that don't belong in `frankenterm-core`.
- **Workspace tally** — `Cargo.toml` `members = [...]` now lists 77 workspace members (28 first-party FrankenTerm crates + 47 vendored `frankenterm/` crates + `fuzz` + `lints/cx_propagation`); the vendored count includes nested `derive` and `lua-api-crates/*` members.
- **One-way edges** — no `frankenterm-core` → sub-crate edges; leaves declare zero first-party deps; cluster sub-crates depend on `frankenterm-core` only.

### RCH worker health + admission (ongoing, ft-ilxky, ft-4tp7g)

RCH (Remote Compilation Helper) hardening continues as a steady drumbeat of fixes; remote-build pressure is the most-blocking class of operator pain.

- **RCH attestation hardening** (ft-ilxky.2) — worker mirror selection pinned on SHA-256 hash equality + e2e nounset propagation hardened ([`0d023d18a`](https://github.com/Dicklesworthstone/frankenterm/commit/0d023d18a)).
- **Sync-check + attest selected worker** before guarded cargo ([`acc18de78`](https://github.com/Dicklesworthstone/frankenterm/commit/acc18de78), [`2fd9bce0a`](https://github.com/Dicklesworthstone/frankenterm/commit/2fd9bce0a)).
- **RCH no-workers admission decomposed** (ft-4tp7g) — 5 blocked sub-beads spelling out exactly which admission predicates fail when no workers pass health ([`ed14598df`](https://github.com/Dicklesworthstone/frankenterm/commit/ed14598df)).
- **rch dry-run summary** correctness fixes recorded as ongoing.

### Doctrine + reality-check (2026-05-01, ft-i2eni)

- **Vendored fork rename** (ft-i2eni.4) — completed the package-name
  rename of the four remaining wezterm-* vendored crates to
  frankenterm-*: `wezterm-client` → `frankenterm-client`,
  `wezterm-font` → `frankenterm-font`, `wezterm-open-url` →
  `frankenterm-open-url`, `wezterm-toast-notification` →
  `frankenterm-toast-notification`. 41 files / 75 substitutions across
  workspace + vendored Cargo.toml manifests + Rust import paths.
  Acceptance: `grep -rE 'name = "wezterm-' frankenterm/*/Cargo.toml`
  returns empty; cargo check passes for all four renamed crates.
- **Doctrine epic retired** (ft-i2eni) — the BR-RC-DOCTRINE epic
  closes with all six children done: `RuntimeProof` sealed trait
  (ft-i2eni.1), `asupersync_test!` declarative macro (ft-i2eni.2),
  cargo-deny tokio bans rule (ft-i2eni.3), this rename (ft-i2eni.4),
  auto-stamped README/AGENTS counts (ft-i2eni.5), and the vendored
  fork PROVENANCE.json manifest (ft-i2eni.6). Code + docs now match
  the stated doctrine end-to-end.

### Ship-readiness refresh (2026-04-26)

- **Robot docs truthfulness** — removed the `ft robot trigger` shipped claim from the README implementation-status table because the current `RobotCommands` surface does not expose that subcommand.
- **Runtime async naming** — retargeted runtime-surface validation from compatibility-era `runtime_compat` names to `runtime_async` names after alias removal.
- **Leaf crate extraction** — split CASS and CAUT data-only types into dedicated leaf crates so core importers can keep shrinking without pulling client/runtime glue across crate boundaries.
- **Terminal protocol correctness** — fixed OSC 8 hyperlink rendering so Display percent-encodes reserved field separators and parse decodes the escaped form back to the original hyperlink.
- **Terminal input correctness** — wired application-keypad mode into termwiz keypad encoding so DECPAM/DECPNM state changes affect numpad escape sequences.
- **Platform behavior hardening** — made native Windows alternate-screen requests report explicit unsupported errors instead of silent success, and fixed Wayland output DPI selection to honor per-screen overrides.
- **Pattern detection precision** — constrained rate-limit retry-duration regex evidence to nearby rate-limit anchors, reducing false positives from unrelated wait/backoff text elsewhere in a pane segment.

## [Pre-0.1.0] -- development on `main` since 2026-02-17

> ~3,500 commits since the `backup-before-rewrite` tag. Active daily development by concurrent agent swarms. The project grew from a WezTerm automation wrapper to a full terminal platform with its own GUI, mux server, and 120-crate workspace (~775k lines of code, 45,000+ tests).

### WezTerm Source Import & FrankenTerm Identity (2026-02-10)

Imported WezTerm source at commit `05343b38` and integrated it as owned code within the workspace. Renamed the project from `wezterm_automata`/`wa` to `frankenterm`/`ft`. All CLI commands, module names, config paths, and documentation updated.

- [Import WezTerm source as FrankenTerm owned code](https://github.com/Dicklesworthstone/frankenterm/commit/e6303733ef911cf7eae8e6c0569a963049315f8c)
- [Integrate FrankenTerm crates into workspace](https://github.com/Dicklesworthstone/frankenterm/commit/09cf50f95fe9524b56b90dfe97fe70d9638a3c56)
- [Rename: wezterm_automata/wa -> frankenterm/ft](https://github.com/Dicklesworthstone/frankenterm/commit/4303a0a32806963d4da1044515c11c416c25e812)
- [Complete wa->ft naming migration in CLI](https://github.com/Dicklesworthstone/frankenterm/commit/bf83c4bc576b5a8072500f153411b337947ca3cf)
- [Replace WezTerm branding with FrankenTerm throughout GUI](https://github.com/Dicklesworthstone/frankenterm/commit/4e9b347af6e28270fb0a035030f9fc57f8948b46)

### Native GUI Terminal (2026-03-02)

Added `frankenterm-gui` crate: a working terminal window that opens natively on macOS, bundled as `FrankenTerm.app` with vendored font rendering. Integrated native event bridge for `ft watch` push-mode observation. TOML-first config with Lua opt-in.

- [Add frankenterm-gui crate to workspace](https://github.com/Dicklesworthstone/frankenterm/commit/07544966c5f7510b885db6b67db908750bafc497)
- [Working terminal window opens on macOS](https://github.com/Dicklesworthstone/frankenterm/commit/7a61af81f439881a9e38fd206aa1a56902e422e0)
- [Build FrankenTerm.app from source, no WezTerm dependency](https://github.com/Dicklesworthstone/frankenterm/commit/8610c04e5d64ccb0d22b36104708c46528c962d1)
- [Add native event bridge emitter for ft watch integration](https://github.com/Dicklesworthstone/frankenterm/commit/35949cd742f564321c56c9498abb19666fc3fe4d)
- [TOML-first config with Lua opt-in and FrankenTerm paths](https://github.com/Dicklesworthstone/frankenterm/commit/4460f6db417749809a2aad0131d97d351bcab98c)
- [Agent-aware session management with state detection and mass operations](https://github.com/Dicklesworthstone/frankenterm/commit/1ea89d500d8558be0c5ae1875803a2135f5fbbe2)
- [Integrated swarm dashboard panel with pane list, health, and events](https://github.com/Dicklesworthstone/frankenterm/commit/74febab420a447477dc715f7929f37ad06a60880)
- [Clamp WebGPU surface dimensions to prevent zero-size panics](https://github.com/Dicklesworthstone/frankenterm/commit/a9e5b076f6f75b35349551c60f00442934c88ee6)
- [Graceful shutdown and RAII cleanup to native event bridge](https://github.com/Dicklesworthstone/frankenterm/commit/2f5ab8da3f0b34682ea74e9184899caa8646dd6e)
- [Per-pane arena byte accounting with peak watermark tracking](https://github.com/Dicklesworthstone/frankenterm/commit/f981a6f998ccd94fb98a6d638a3eb56e9582220f)

### Mux Server & Workspace Vendoring (2026-03-02)

Vendored GUI layer crates and mux server into the workspace. Added swap layouts, floating pane toggle, stack cycling, and SSH domain config.

- [Add frankenterm-mux-server binary and library](https://github.com/Dicklesworthstone/frankenterm/commit/81ec52a09dd3c64382779f30eb705de1bbc4b1f6)
- [Vendor GUI layer crates into workspace](https://github.com/Dicklesworthstone/frankenterm/commit/dfd875e7df23ad1802e1dba9d3786bdd52860936)
- [Add swap layouts, floating pane toggle, and stack cycling actions](https://github.com/Dicklesworthstone/frankenterm/commit/2d422a37aa4a73856e7894b658ca5990702aece6)
- [PDU handlers for layout swap, cycle, and stack operations](https://github.com/Dicklesworthstone/frankenterm/commit/4c907ff1221704f3f37841ab6a4a8c6f4246cc5e)
- [jemalloc as default allocator via frankenterm-alloc](https://github.com/Dicklesworthstone/frankenterm/commit/8222b2c7edb5be6a95791ee222c1f4d354e69d23)
- [SSH domain config docs and TOML parse tests](https://github.com/Dicklesworthstone/frankenterm/commit/72ff36b1debb4271bc1c77ff851ec6d95aac5e5f)

### Native Mux Lifecycle (2026-03-02 -- 2026-03-03)

Ground-up native mux subsystem: lifecycle state machine with concurrency control, command transport primitives, topology orchestration, session profiles/templates, durable state checkpoints with rollback, headless/federated mux server for remote fleet control, and connector host runtime.

- [Native mux lifecycle state machine](https://github.com/Dicklesworthstone/frankenterm/commit/ab8928de2170cf2b7d938fd65b3abe69dec2fb34)
- [Command transport primitives](https://github.com/Dicklesworthstone/frankenterm/commit/1386cfbfd6265d44f7c165e6b8cdd6196d6c8ba6)
- [Topology orchestration service](https://github.com/Dicklesworthstone/frankenterm/commit/16d2ebcf2cde252704d0b618922f2308fac5f676)
- [Session profile/template/persona engine](https://github.com/Dicklesworthstone/frankenterm/commit/d282b89e6fd12b732f65742491841ae925f8659d)
- [Durable state checkpoint/rollback subsystem](https://github.com/Dicklesworthstone/frankenterm/commit/e0733bea4089db2432c175f1834098f133d41405)
- [Headless/federated mux server for remote fleet control](https://github.com/Dicklesworthstone/frankenterm/commit/228ba05fdf1b08b256195c5b6c972d02786913b5)
- [Connector host runtime lifecycle and protocol envelope](https://github.com/Dicklesworthstone/frankenterm/commit/dacf410f2c868862cefc8192fbcaec423a3b333c)

### Swarm Orchestration Runtime (2026-03-03)

Purpose-built fleet management for 200+ concurrent AI agents: deterministic launch plans with phased startup and weighted ordering, dependency-aware work queues with anti-starvation fairness, Agent Mail coordination kernel, and swarm pipeline for DAG-ordered orchestration.

- [Deterministic fleet launch plan with phased startup](https://github.com/Dicklesworthstone/frankenterm/commit/3052b1822f6f5ecff098cde452abb4cd3a98acaa)
- [Dependency-aware swarm work queue with anti-starvation](https://github.com/Dicklesworthstone/frankenterm/commit/4f68ec851970fddab642201e46910792c8a691db)
- [Mission Agent Mail coordination kernel](https://github.com/Dicklesworthstone/frankenterm/commit/9fbf9503a91d30e2426310e86589eb48a6e3f466)
- [Swarm pipeline for DAG-ordered fleet orchestration](https://github.com/Dicklesworthstone/frankenterm/commit/a94bf4fece31ab0bab4e7d715a408ee34d9e17bf)
- [Swarm command center dashboard and command palette](https://github.com/Dicklesworthstone/frankenterm/commit/8acd9ae7f57fd75e35fcf420175263c42052bd4d)
- [Resource locking, deadlock detection, and safe agent handoff](https://github.com/Dicklesworthstone/frankenterm/commit/a825db4b3c58602999484a9166b64b8a2d25e009)
- [Streaming event/wait interfaces for deterministic automation](https://github.com/Dicklesworthstone/frankenterm/commit/ee808973276744640e2068914d521c189dda20b9)

### Connector SDK & Extension System (2026-03-03)

Multi-host connector mesh federation, connector SDK with builders/linting/certification/simulator, inbound/outbound bridges, canonical event schema with evolution tooling, and circuit-breaker reliability.

- [Multi-host connector mesh federation](https://github.com/Dicklesworthstone/frankenterm/commit/5c1993892c8d112183ad62d1ff120c6805ccc89f)
- [Connector SDK devkit with certification pipeline](https://github.com/Dicklesworthstone/frankenterm/commit/67eee8318df9d5da1976c5aaadd3fbb2c123c5e4)
- [Connector outbound bridge action routing](https://github.com/Dicklesworthstone/frankenterm/commit/602df4d7d52efd0dec94a84209133c1152e44713)
- [Sandbox capability envelope and zone enforcement](https://github.com/Dicklesworthstone/frankenterm/commit/6634aea6a6dc96f9d41437f100f5bee2432e8c78)
- [Canonical event schema with evolution tooling](https://github.com/Dicklesworthstone/frankenterm/commit/5278b80ad1dd889fd1129570829b73031cad6896)
- [Circuit-breaker, DLQ, and replay controls for connector reliability](https://github.com/Dicklesworthstone/frankenterm/commit/f0f6fc187074ff6d9883cb85c92ad4c777586d4b)
- [Connector credential broker for policy-aware secret provisioning](https://github.com/Dicklesworthstone/frankenterm/commit/6e5fa61b76004e3ef13471bef93ad2a1c2af0b19)
- [Bundle registry and connector testbed with chaos scenarios](https://github.com/Dicklesworthstone/frankenterm/commit/88067507cf210f89a30c46598129b8da5959c246)

### 21-Subsystem Policy Engine (2026-03-10)

Expanded the policy engine from basic `authorize()` to a unified governance framework integrating 21 subsystems: quarantine registry, kill-switch, hash-linked tamper-evident audit chain, compliance reporting, credential broker, connector governor, namespace isolation, approval tracker, revocation registry, and forensic report generator.

- [Quarantine registry and kill-switch primitives](https://github.com/Dicklesworthstone/frankenterm/commit/d33b8a1d927fb15d616ed28c6be3124236b36732)
- [Hash-linked tamper-evident audit chain](https://github.com/Dicklesworthstone/frankenterm/commit/f37d2bf5bce393159d6d7b8cdcf447705da8836d)
- [Compliance reporting engine](https://github.com/Dicklesworthstone/frankenterm/commit/4f58605c3a329d696fa6e7461972f0acd9dc62cb)
- [Credential broker integration](https://github.com/Dicklesworthstone/frankenterm/commit/bcfe80193e3e7ef4473f91c07599cff00d567d56)
- [Namespace isolation for multi-tenant connectors](https://github.com/Dicklesworthstone/frankenterm/commit/144cf6bc5b4bf3a5af03ae894b1827d6e95abef1)
- [Forensic report generator with query/export pipeline](https://github.com/Dicklesworthstone/frankenterm/commit/34bcc891a5a42fdb8bbc785b947e5f21bef74c27)
- [Policy metrics aggregation and health dashboard](https://github.com/Dicklesworthstone/frankenterm/commit/20bf9860479cf973b0c3b964c0ea62d8a57b0732)
- [PolicySurface dimension for subsystem-level rule matching](https://github.com/Dicklesworthstone/frankenterm/commit/752447c5f90d4a0ead196c30aa1733235819b0e7)
- [Approval tracker with revocation registry](https://github.com/Dicklesworthstone/frankenterm/commit/d6470114de35d42250ad0569a4637182020c649f)

### Transaction Execution Engine (2026-03-13 -- 2026-03-17)

Multi-pane transactional operations with prepare/commit/compensate lifecycle, idempotency guards, deterministic replay, mission journal with compaction, and `ft tx` CLI subcommand.

- [Tx execution engine: prepare/commit/compensate lifecycle](https://github.com/Dicklesworthstone/frankenterm/commit/f1f129300ee4b2db63a13e8159899f418bd8cece)
- [ft tx subcommand for mission transaction control](https://github.com/Dicklesworthstone/frankenterm/commit/62d3fc446bfe46745661dc2684487845c08e2d4f)
- [Crash-consistent mission journal with compaction and replay](https://github.com/Dicklesworthstone/frankenterm/commit/6fee81d1a9f16405884d24eedd5a34c7de34af43)
- [Mission pause/resume/abort with checkpoint persistence](https://github.com/Dicklesworthstone/frankenterm/commit/2b3c2843783c9766a58a084ecca70c78d85d80b0)
- [Harden resume safety, compensation step results, persist contract state](https://github.com/Dicklesworthstone/frankenterm/commit/3665ca9b2bb458c1dfa3b2b44dee080b60a60c47)
- [Mission abort-with-checkpoint, lock lease renewal, snapshot metadata](https://github.com/Dicklesworthstone/frankenterm/commit/fe3583b9528ac881d04d519cdd0119079d3dfef4)
- [Require commit receipts for rollback instead of assuming all steps committed](https://github.com/Dicklesworthstone/frankenterm/commit/d9a5ded1227972d456beb79e9c52a1f964baa529)
- [Failed state made non-terminal so it can transition to Compensating](https://github.com/Dicklesworthstone/frankenterm/commit/6d2870f295e424e890b5a97c878407d24d94da91)
- [SHA-256 deterministic tx key hashing across processes](https://github.com/Dicklesworthstone/frankenterm/commit/55bcd5f9cb68a91b9271f7b646835d188423f9a2)
- [TxRollback surface added to ApiSurface contract](https://github.com/Dicklesworthstone/frankenterm/commit/d3327739b0b78f1c88dcc32487ab495667986f68)

### Tiered Scrollback & Fleet Memory Controller (2026-03-12)

Three-tier memory management (hot/warm/cold) for 200+ pane workloads. Unified fleet memory controller synthesizing backpressure from queue depth, system memory, and per-pane budgets with hysteresis.

- [Tiered scrollback storage for 200+ pane agent swarms](https://github.com/Dicklesworthstone/frankenterm/commit/ba9fc94987150304c5d506c7b2d147e3a654a9ba)
- [Fleet memory controller unifying 5 memory subsystems](https://github.com/Dicklesworthstone/frankenterm/commit/b6b93b86d6dd6bd12d780d06a92df46823ac4fb8)
- [Per-pane cost aggregation with budget alerts](https://github.com/Dicklesworthstone/frankenterm/commit/5948dfacd84da4984a0ebccf22fad14c41bddb3f)
- [Pre-launch quota gate for pane spawning](https://github.com/Dicklesworthstone/frankenterm/commit/3fddf4696fdd8512b72b52445ccfe1f9531d6be8)
- [Per-pane arena byte accounting with peak watermark](https://github.com/Dicklesworthstone/frankenterm/commit/f981a6f998ccd94fb98a6d638a3eb56e9582220f)
- [Core-level swarm stress tests for 200-pane workloads](https://github.com/Dicklesworthstone/frankenterm/commit/2f7f73c18809773a3845ae09c5ba1a95cf6a7bbf)

### Distributed Mode Hardening (2026-03-11 -- 2026-03-17)

Protocol version validation on handshake, gap cursor seeding with interleaved chronological replay, session-scope tracking with reconnect cleanup, stale scope pruning with heartbeat tracking, and session checkpoint save/restore.

- [Protocol version validation on handshake](https://github.com/Dicklesworthstone/frankenterm/commit/287efbe432d1eec71e089bc55ea8e3d760e347b4)
- [Gap cursor seeding and interleaved chronological replay](https://github.com/Dicklesworthstone/frankenterm/commit/bd9cadabe8dabf4260e938ede636e311ac2371d1)
- [Session-scope tracking with reconnect cleanup](https://github.com/Dicklesworthstone/frankenterm/commit/120e79375b0100975bce117d613245307b275a63)
- [Session checkpoint save/restore with aggregator state](https://github.com/Dicklesworthstone/frankenterm/commit/1b3d0aed30cd3400eb44b510b550049f4f72e309)
- [Stale scope pruning with listener heartbeat tracking](https://github.com/Dicklesworthstone/frankenterm/commit/16076688320829c5523643dc040c39891acd0ad8)
- [Local receipt clock for stale-agent pruning (untrusted remote clocks)](https://github.com/Dicklesworthstone/frankenterm/commit/74d6f62a40973868fc9616a42b7fe88b9068afe1)
- [Constant-time comparison for identity validation](https://github.com/Dicklesworthstone/frankenterm/commit/9d2d038f9156b9676dc0b3b8d64fe21f475a88ba)
- [Validate PaneDelta content_len matches actual content length](https://github.com/Dicklesworthstone/frankenterm/commit/79731b1b5ab62a4860f3734d8f7cbecdc78182a4)
- [Harden security error responses to avoid info leakage](https://github.com/Dicklesworthstone/frankenterm/commit/e138ed36241238cb0d686320afb302ec2ade7d02)

### Replay & Forensics (2026-02-06 -- 2026-03-17)

Sensitivity tiers, redaction policy, and causal chain fields for replay events. Deterministic replay with canonical string methods for tx operations.

- [Replay engine for session recordings](https://github.com/Dicklesworthstone/frankenterm/commit/714ec19f71e766dd01908b0da9383d63c39fa277)
- [Recording export with HTML player, Asciinema cast, and redaction](https://github.com/Dicklesworthstone/frankenterm/commit/0719dfe9dc6d88d29695028b1a70e782339ae125)
- [Add sensitivity tiers, redaction policy, and causal chain fields](https://github.com/Dicklesworthstone/frankenterm/commit/8b8ec5c6f136ee321f3376be8d000f33677cb4d2)
- [Add PartialEq/Eq derives and canonical_string methods for tx replay](https://github.com/Dicklesworthstone/frankenterm/commit/05a3a8bd79b5affca4cc693f72b103473802f3a9)
- [Forensic export pipeline types and query engine](https://github.com/Dicklesworthstone/frankenterm/commit/78fc004369187b61305829884996acb00b89dc75)

### CASS Export (2026-03-13 -- 2026-03-20)

CASS integration workflows and new `cass-export` feature for exporting recorder sessions to CASS connectors.

- [HandleOnErrorCassSearch handler for cass-based error recovery](https://github.com/Dicklesworthstone/frankenterm/commit/f272d73f6e34f2a1e47a69462168d5f623a9cc9b)
- [HandleSwarmLearningIndex with CassClient.trigger_index](https://github.com/Dicklesworthstone/frankenterm/commit/ae6d9da53c75b612a9167a7dcb3f160fa4d24591)
- [Add cass-export feature](https://github.com/Dicklesworthstone/frankenterm/commit/a84f6fcd48d9df1ca51e04fe38f539a7a822bcab)
- [Correct token estimation for whitespace-only splits](https://github.com/Dicklesworthstone/frankenterm/commit/1cd36ab36bd2d7ac036e90018088543735fe8ea5)

### Async Runtime Migration: tokio -> asupersync (2026-02-11 -- 2026-03-21)

Systematic migration from tokio to the asupersync runtime with `runtime_compat` abstraction layer. All `#[tokio::test]` tests migrated. Benchmarks migrated. Feature-gated dual-runtime compatibility surface maintained during transition.

- [asupersync-runtime flag and cx_creation bench](https://github.com/Dicklesworthstone/frankenterm/commit/8d293835b582906e701589c587d711f1ff2f1594)
- [Runtime abstraction layer for asupersync migration](https://github.com/Dicklesworthstone/frankenterm/commit/7832fd757039b33c8872afc91dfc4f7e618c1a3f)
- [Enable all features by default, migrate test suite](https://github.com/Dicklesworthstone/frankenterm/commit/b66eb6a60caa5a83d7f4bd2235350161fcc86a94)
- [Migrate all 111 #[tokio::test] to asupersync compat runtime](https://github.com/Dicklesworthstone/frankenterm/commit/2482469a876164f0ef6604fd7046cd9ba7408dfd)
- [PTY layer migrated from smol to asupersync](https://github.com/Dicklesworthstone/frankenterm/commit/e8537476dd89e29fcbde73cca8499c273f07c31d)
- [Close ft-e34d9 epic: tokio->asupersync migration COMPLETE](https://github.com/Dicklesworthstone/frankenterm/commit/2f3b2891ce6df161d3eefaa5060e95ef85479728)
- [Replace async-io and async-channel with promise and flume](https://github.com/Dicklesworthstone/frankenterm/commit/183f04dc58df37e1cd15bd34033f4c4a220f6a74)
- [Unify mux I/O through runtime_compat::io](https://github.com/Dicklesworthstone/frankenterm/commit/d432db35f83afe3fd1a3671da085f240d0c9fa21)
- [Replace smol::Timer and smol::block_on with promise::spawn in GUI](https://github.com/Dicklesworthstone/frankenterm/commit/bdcf9ff8e3de8cc39c4cf5f1fdecedf69aff0cf2)

### WASM Extension System (2026-02-13)

WASM extension sandbox with security model, module cache, host function API, FrankenTerm Extension Package Format (.ftx), extension lifecycle management, and event bus/keybinding/storage APIs.

- [WASM extension sandbox security model](https://github.com/Dicklesworthstone/frankenterm/commit/8208656979a255625cdd0853677fbc6c17d04fca)
- [FrankenTerm Extension Package Format .ftx](https://github.com/Dicklesworthstone/frankenterm/commit/a0211244a9277c126e1e98e2efa2beaf7234b70e)
- [Extension lifecycle management](https://github.com/Dicklesworthstone/frankenterm/commit/26b8540f520186e03f94045079e769bf4522d233)
- [Config migration tool: wezterm.lua to frankenterm.toml](https://github.com/Dicklesworthstone/frankenterm/commit/37e4063191b6ad2203dba1f8f05075d11775d44f)

### Session Persistence & Restore (2026-02-10)

Complete session persistence stack: pane state snapshots, topology serializer, SnapshotEngine orchestrator, layout restoration engine, session retention policy, session restore from unclean shutdowns, and process re-launch engine.

- [Session persistence: pane state snapshots and topology serializer](https://github.com/Dicklesworthstone/frankenterm/commit/038d2f1e808c84fe2f171b9c4a09ff53bff15292)
- [SnapshotEngine orchestrator](https://github.com/Dicklesworthstone/frankenterm/commit/56f0fc79758f0135767309ed6642501bdd3161e6)
- [Layout restoration engine](https://github.com/Dicklesworthstone/frankenterm/commit/72717c857b23e85bc839369b9929850ce72cbf69)
- [Session restore engine: detect and recover from unclean shutdowns](https://github.com/Dicklesworthstone/frankenterm/commit/bfd0a82edf85e6700121f302baf50ff52d2da90f)
- [Process re-launch engine](https://github.com/Dicklesworthstone/frankenterm/commit/76f7949bb716be685a2934a33582bc93fca58d42)
- [ft session CLI subcommands](https://github.com/Dicklesworthstone/frankenterm/commit/cbbfec73442b35f5d8b36bacbc33a8ce36e0b23d)

### FTUI Migration (2026-02-08 -- 2026-02-09)

Complete TUI rewrite: one-writer output routing, app shell with Model impl, canonical keybinding table, input dispatcher, command execution state machine, migrated Events/Triage/History/Search/Help views, chaos tests, runtime backend selection for phased rollout.

- [FTUI migration foundation (FTUI-01 through FTUI-04.2)](https://github.com/Dicklesworthstone/frankenterm/commit/ecb7684264f931baf489ffa1106a422f9cc2e9fb)
- [Canonical keybinding table and input dispatcher](https://github.com/Dicklesworthstone/frankenterm/commit/ef78f349f7a4ad31f2161e5c443a89bdfdae9c8e)
- [Migrate Triage view with ranked items and workflow panel](https://github.com/Dicklesworthstone/frankenterm/commit/f09a06665dc2448778d3eab90fab4d665b6d697c)
- [Runtime backend selection for phased rollout](https://github.com/Dicklesworthstone/frankenterm/commit/f1a09e1a087462807398eaabcb5d86ae4adcddb5)
- [Interactive timeline view with zoom and responsive layout](https://github.com/Dicklesworthstone/frankenterm/commit/95dc7463af4af7990190b6aaf77902452762c99b)
- [Responsive breakpoints for Events, History, Search, Triage, Help views](https://github.com/Dicklesworthstone/frankenterm/commit/f4ef2d39148c4ebd9f6bd6b808c5ce598e3eecb9)
- [Dashboard state aggregator with summary line for status bar](https://github.com/Dicklesworthstone/frankenterm/commit/2a2715ad6c1401fd6a573c6feee4f656f47f202e)

### Probabilistic Intelligence Engine (PIE) (2026-02-11)

Advanced statistical and ML subsystems for agent behavior analysis: Bayesian Online Change-Point Detection (BOCPD), conformal prediction, cross-pane correlation with chi-squared co-occurrence, adaptive Kalman filter watchdog thresholds, ADWIN pattern drift detection, Bayesian evidence ledger, LSH error clustering, causal DAG with transfer entropy, session DNA behavioral fingerprinting, spectral FFT agent classification, MaxEnt IRL preference discovery, and VOI-optimal capture scheduling.

- [Bayesian Online Change-Point Detection](https://github.com/Dicklesworthstone/frankenterm/commit/df01ece958d6fe040f953fd3e0e78f8e5e834f4e)
- [Cross-pane correlation engine with chi-squared](https://github.com/Dicklesworthstone/frankenterm/commit/49a972c812e524013122d57f6fd547fabbc42115)
- [Causal DAG with transfer entropy](https://github.com/Dicklesworthstone/frankenterm/commit/260e9d2752e000fb87e609eebf4c25e816436128)
- [Session DNA behavioral fingerprinting with PCA](https://github.com/Dicklesworthstone/frankenterm/commit/ab30676b9f68cee704e5f41e1a00295163fa09df)
- [Spectral fingerprinting via FFT for agent classification](https://github.com/Dicklesworthstone/frankenterm/commit/51fd6060e2b7e313dd0a8f3c49005918baf88986)

### Search & Indexing Expansion (2026-02-19 -- 2026-02-21)

FrankenSearch subsystem: configurable fusion backend selector, embedding daemon server/worker, incremental document indexing pipeline, Tantivy-based lexical search service, daemon wire protocol, and WAL with CRC32 integrity and crash recovery.

- [Configurable fusion backend selector](https://github.com/Dicklesworthstone/frankenterm/commit/f86e760a04947c1c487a0a3b940db35c470e3e53)
- [Embedding daemon server and worker](https://github.com/Dicklesworthstone/frankenterm/commit/9e950a155cda968c833b98d422ae16d5923008d5)
- [Incremental document indexing pipeline](https://github.com/Dicklesworthstone/frankenterm/commit/1315fcea9960ec0f44dd02de54c450684ec67324)
- [WAL with CRC32 integrity and crash recovery](https://github.com/Dicklesworthstone/frankenterm/commit/12047c3c965e7050e84ffdf6344c66a5f40d96da)
- [TantivySearchService implementing LexicalSearchService trait](https://github.com/Dicklesworthstone/frankenterm/commit/46c0f9f053d45addd4fe2f1a32b8dddf3615bb63)

### Streaming Output & Mux Pool (2026-02-08 -- 2026-02-10)

Streaming output subscription from WezTerm mux, DirectMuxClient connection pool, mux watchdog integration, and backend-backed `get_text`/`list_panes`/`send_text`.

- [Streaming output subscription from WezTerm mux](https://github.com/Dicklesworthstone/frankenterm/commit/9b58b03be25f6025e3d5208823fcb4b265961346)
- [DirectMuxClient connection pool](https://github.com/Dicklesworthstone/frankenterm/commit/e7c06dde25917e3723bc5ed766a7a8a1ec3d5353)
- [Wire mux watchdog into watcher](https://github.com/Dicklesworthstone/frankenterm/commit/08b2d9fd4f3c6e6b30594af760e7995ac6a127f6)
- [RAII MuxSubscriptionGuard to prevent subscription leaks](https://github.com/Dicklesworthstone/frankenterm/commit/ffc814429896fcd100c8b8bdf4ed3c913af0bdc3)
- [CxScope RAII guard for context lifecycle and MuxPool health check timeout](https://github.com/Dicklesworthstone/frankenterm/commit/2f5ed9bf85680bc4236049234ec9891a007b2a17)

### MCP Server Surface (2026-02-06 -- 2026-03-14)

MCP resource layer for agent introspection, URI-template resources, audit recording, tool-level agent filtering, framework-neutral types, and machine contracts with SDK generation.

- [MCP resource layer for agent introspection](https://github.com/Dicklesworthstone/frankenterm/commit/28ff3f745d440b2f0cb579f374ad04e55b454462)
- [URI-template resources and resource helpers](https://github.com/Dicklesworthstone/frankenterm/commit/a32f7819311ae186f9fa2ded0a8e25d607ecc28e)
- [MCP send command with policy gating and workflow integration](https://github.com/Dicklesworthstone/frankenterm/commit/5741fbdf4b724abd57e80f141299bbee0d207b5c)
- [Framework-neutral tool and content types at client boundary](https://github.com/Dicklesworthstone/frankenterm/commit/3d37ab51b6dda46d68e6c17980d855d782e4fc02)
- [Machine contracts, SDK generation, and NTM-compat shim](https://github.com/Dicklesworthstone/frankenterm/commit/e694341cba1c2343e55d366a247ce42be5b09ec6)
- [Runtime_compat surface contract expanded: broadcast/oneshot/notify (15->18)](https://github.com/Dicklesworthstone/frankenterm/commit/e20a34514eb23b157f42526aa088b41599048bcf)

### Recording Engine & Secrets Scanner (2026-02-04)

Recording engine for session capture, secrets scanner with incremental checkpoint/resume, and Prometheus metrics endpoint.

- [Recording engine, secrets scanner, and incremental segment scan](https://github.com/Dicklesworthstone/frankenterm/commit/4f08087c3ce05aafd99ce329864a30d6c459c9bb)
- [Incremental scan with checkpoint/resume and schema v13 migration](https://github.com/Dicklesworthstone/frankenterm/commit/0dfcbded903dafc0c8735cd448a8703b92bd5e13)
- [Prometheus metrics endpoint support](https://github.com/Dicklesworthstone/frankenterm/commit/58e1ef1b2f81d658be2989d7c37cfb9df09610eb)
- [Input-to-display latency measurement framework](https://github.com/Dicklesworthstone/frankenterm/commit/d5f7e105b9958ba9521f4cae0a5336e5850f6cd8)

### IPC Authentication (2026-02-04)

Token-based authentication with scopes and expiry for IPC socket connections, plus RPC handler framework.

- [Token-based authentication with scopes and expiry](https://github.com/Dicklesworthstone/frankenterm/commit/773bca0672bdb7f649ac3ae083bf526262957ac1)
- [RPC handler framework and IPC client enhancements](https://github.com/Dicklesworthstone/frankenterm/commit/2a8bda4711696209ad0a23d2929d425d8d4b11ef)

### Data Structures Library (2026-02-12 -- 2026-02-22)

Comprehensive set of probabilistic and algorithmic data structures: bloom filter, ring buffer, reservoir sampler, token bucket rate limiter, exponential histogram, sharded counters, concurrent map, entropy accounting, homomorphic stream hashing, count-min sketch, cuckoo filter, HyperLogLog++, t-digest, skip list, Merkle tree, WAL engine, persistent immutable data structures, convergent reconciliation protocol, bimap, sliding window, compact bitset, time series, edit distance, topological sort, shortest path, Fenwick tree, segment tree, union-find, XOR filter, and latency model with network calculus.

- [Bloom filter](https://github.com/Dicklesworthstone/frankenterm/commit/cd0dbbe80fd3221feaa014245f6a893cc4050980)
- [Token bucket rate limiter](https://github.com/Dicklesworthstone/frankenterm/commit/edc2a0510a966ad67b47bf183b6d87ef60604817)
- [Cuckoo filter with deletion support](https://github.com/Dicklesworthstone/frankenterm/commit/1edc8592a480421ef7624d700acb66c682e61bce)
- [HyperLogLog++ approximate distinct count](https://github.com/Dicklesworthstone/frankenterm/commit/047d40815ba3a246791d29fe9ce428a3e16cb0dd)
- [T-digest streaming percentile estimation](https://github.com/Dicklesworthstone/frankenterm/commit/627a57ac83fa2b60447d06d86dbd56dde1a48800)
- [Merkle tree for state reconciliation](https://github.com/Dicklesworthstone/frankenterm/commit/de7cf97eb07884061e286257bdd5b0b8e2ae0651)
- [Write-ahead log with proptest coverage](https://github.com/Dicklesworthstone/frankenterm/commit/5a8060e61a0ce3225d12bd4c8372c44bf97c8f6b)
- [Persistent immutable data structures with structural sharing](https://github.com/Dicklesworthstone/frankenterm/commit/f00415455628ea8740fdb7f721443a404efe0f11)
- [Convergent reconciliation protocol](https://github.com/Dicklesworthstone/frankenterm/commit/058ab9b82bcd58b2294478f8f0b8982202fa91db)
- [Latency model: network calculus for formal worst-case guarantees](https://github.com/Dicklesworthstone/frankenterm/commit/70f159e68a7a5c9337530dd7ebd455f619b4847d)
- [Topological sort with graph algorithms](https://github.com/Dicklesworthstone/frankenterm/commit/4d3d9488040adb6483b0351ea58a7525d7364d2b)

### Latency Budget Framework (2026-02-23)

Latency stage decomposition, budget algebra, BudgetEnforcer, instrumentation probes with correlation context, adaptive budget allocator, three-lane scheduler with admission policy, bounded input ring with backpressure, priority inheritance, starvation prevention, zero-copy ingestion parser, and tail-latency controller.

- [Latency stage decomposition and budget algebra](https://github.com/Dicklesworthstone/frankenterm/commit/36752fb60baf799b85007ff1589c9e49a45fddf6)
- [Three-lane scheduler with admission policy](https://github.com/Dicklesworthstone/frankenterm/commit/524d24ee0f1ac71df8c1b6deeade7d1d10cd5114)
- [Zero-copy ingestion parser with line boundary detection](https://github.com/Dicklesworthstone/frankenterm/commit/9dc6e36361c604c462452d6bd73610698b2168d9)
- [Kernel/hardware tail-latency controller](https://github.com/Dicklesworthstone/frankenterm/commit/6431a258815ff6e56e42bee564d3301290b10ebf)

### Resize Subsystem (2026-02-13 -- 2026-02-14)

Resize scheduler with transaction state machine, cross-pane storm detection, domain-aware throttling, memory-pressure-aware controls, crash forensics with bundle integration, wrap quality scorecard, and resize dashboard.

- [Resize scheduler with transaction state machine](https://github.com/Dicklesworthstone/frankenterm/commit/2b42b9d8041467fc064a8a8a7b5d55d32f537454)
- [Cross-pane storm detection and domain-aware throttling](https://github.com/Dicklesworthstone/frankenterm/commit/8f7ce850a189c63ae5250c368db755cac6bf4743)
- [Resize crash forensics module](https://github.com/Dicklesworthstone/frankenterm/commit/259be6ba338a9602fa87b8f3af2462548e94b1ca)
- [Resize dashboard renderer with risk diagnostics](https://github.com/Dicklesworthstone/frankenterm/commit/f0486a913f049c44f5f47f0201b5b98687d0480f)
- [Wrap quality scorecard and readability gate enabled by default](https://github.com/Dicklesworthstone/frankenterm/commit/27c3677adf8fcd127bee629baaf46af25d465dc5)

### Wire Protocol & Distributed Aggregator (2026-02-09)

Wire protocol aggregator for agent stream dedup and ingest, user pattern packs with custom namespace prefixes.

- [Aggregator for agent stream dedup and ingest](https://github.com/Dicklesworthstone/frankenterm/commit/204bf242031f0c6b2242250e7b2ff64a3922d982)
- [User pattern packs with custom namespace prefixes](https://github.com/Dicklesworthstone/frankenterm/commit/f63983088655528b0cbe6eb904db058383aff15d)
- [Token sources with rotation and doctor integration](https://github.com/Dicklesworthstone/frankenterm/commit/2fb5155ec34465e8b7e4ac862dce28b7e09f6857)

### Operational Telemetry (2026-02-10 -- 2026-03-11)

Per-pane process tree capture, FD budget tracking, memory pressure engine with tier-based actions, operational telemetry pipeline, Weibull survival model for mux health prediction, differential snapshot system, unified telemetry schema, fleet dashboard and alerting, capacity governor, disaster recovery drill framework, and runtime SLOs.

- [Per-pane process tree capture](https://github.com/Dicklesworthstone/frankenterm/commit/90798df8758c714ee6eddfd66e5fdadc581817fe)
- [Memory pressure engine with tier-based actions](https://github.com/Dicklesworthstone/frankenterm/commit/98b6c349099bc518f3218f830e57c5ebdf56f340)
- [Weibull survival model for mux health prediction](https://github.com/Dicklesworthstone/frankenterm/commit/fab3dbaaac63f1440ab7a7d90c5c8bce19425cd8)
- [Differential snapshot system](https://github.com/Dicklesworthstone/frankenterm/commit/06742f973638a1b3f72546bbfa78dd2500ffa00a)
- [Unified telemetry schema module](https://github.com/Dicklesworthstone/frankenterm/commit/70156db1fe978addb65c1f2c4ffa0aba2ad806bf)
- [Fleet dashboard and alerting module](https://github.com/Dicklesworthstone/frankenterm/commit/6862ac1a79f771bf78f00c7b4f7118c68ddd8523)
- [Capacity governor with rch-aware workload control](https://github.com/Dicklesworthstone/frankenterm/commit/87cc28612e9b1be27e2b57a7210fdd93cefdabca)
- [Disaster recovery drill framework with RTO/RPO scoring](https://github.com/Dicklesworthstone/frankenterm/commit/b0d1641ec0bafa215f59a4a46c64152bad26b8a7)
- [Runtime SLOs, alert policies, and automated gate evaluation](https://github.com/Dicklesworthstone/frankenterm/commit/d462182a0aa9361535dce641f9b5df08b2eec169)
- [Decision trace console for operator-facing explainability](https://github.com/Dicklesworthstone/frankenterm/commit/a285c8bfc04f4ed3e73d86980df7137b02b52c94)
- [Session/workflow explorer with timeline replay and extraction](https://github.com/Dicklesworthstone/frankenterm/commit/453de189790b8b0d589876935529b1d0bd3890cf)

### Undo/Redo Framework (2026-02-08)

Undo/redo framework for reversible workflow actions with `ft undo` subcommand.

- [Undo/redo framework for reversible workflow actions](https://github.com/Dicklesworthstone/frankenterm/commit/fa91a9de78760582a865995807a5334d8b244c7f)
- [ft undo subcommand](https://github.com/Dicklesworthstone/frankenterm/commit/d77d5353b01c0447397350742a46efd06976d923)

### Robot Mode Expansion (2026-03-11 -- 2026-03-16)

- [NTM-compatible command aliases for common robot subcommands](https://github.com/Dicklesworthstone/frankenterm/commit/1cf937155bcf401e8305aa26e4aa09dfb74042ed)
- [NTM-gap command families wired into CLI dispatch](https://github.com/Dicklesworthstone/frankenterm/commit/767d0e8b83db9dbe7647f6fbd709a341e00d64ce)
- [Robot agents configure command with dry-run support](https://github.com/Dicklesworthstone/frankenterm/commit/1af09c8d87cfa01dd1bebffc3a30d286bdec1497)
- [Robot idempotency guard for safe mutation retries](https://github.com/Dicklesworthstone/frankenterm/commit/20c443a05415fa097602d549214905bc9a2a11c6)
- [Forward-compatible error code string newtype replacing enum](https://github.com/Dicklesworthstone/frankenterm/commit/2c76b7edb580536cfebce7ab38cbac7b29b34ce9)
- [Expanded error code catalog with workflow_aborted and workflow_error](https://github.com/Dicklesworthstone/frankenterm/commit/62ef348f07b63d1b69ea154fd4d8031efe856cf3)

### Comprehensive Proptest Coverage (2026-02-14 -- 2026-03-20)

Hundreds of property-based tests added across every major subsystem: search bridge, indexing, connectors, swarm pipeline, CASS types, async boundary contracts, and many more. Over 45,000 tests total.

- [Massive test expansion wave: hundreds of inline tests expanded across 60+ modules (wa-1u90p.7.1)](https://github.com/Dicklesworthstone/frankenterm/commit/835aca05f2e6cef808a5eaae107e7d5868f0e5cc)
- [43-test proptest suite for swarm pipeline](https://github.com/Dicklesworthstone/frankenterm/commit/c8673900114f02d910d7480750e9e51105043768)
- [39 proptest serde roundtrips for uncovered cass types](https://github.com/Dicklesworthstone/frankenterm/commit/3455568402ddf46cc467edcf334527f8230b37c7)
- [24 behavioral runtime tests for core/vendored async boundary](https://github.com/Dicklesworthstone/frankenterm/commit/b604f80e03ad04ff1c7167a674fe413ea82639ae)
- [200-pane swarm stress tests](https://github.com/Dicklesworthstone/frankenterm/commit/2f7f73c18809773a3845ae09c5ba1a95cf6a7bbf)
- [RCH fail-closed guards across all 73 E2E harnesses](https://github.com/Dicklesworthstone/frankenterm/commit/66aee00a4f2c68b1fefe41ff54a5be5460945a34)

### Bug Fixes (selected)

- [Resolve ft status panic and ft watch lifecycle crash](https://github.com/Dicklesworthstone/frankenterm/commit/bbdbfdfbdc98801b00d8a826849bea92f171e148)
- [Harden byte compression, runtime abort, session resume, tx execution](https://github.com/Dicklesworthstone/frankenterm/commit/d961feaae53abcf631cc002422fbe1f7bb40912c)
- [Rate limit off-by-one, silent compression failure, silenced ledger errors](https://github.com/Dicklesworthstone/frankenterm/commit/c948158e44cbe074dc4018817568a390927ea0c1)
- [Harden wire protocol, storage, and session restore with strict validation](https://github.com/Dicklesworthstone/frankenterm/commit/bdd966091dc864be1885b1723c10fc20ef8528b4)
- [EventBus deadlock when handler calls deregister](https://github.com/Dicklesworthstone/frankenterm/commit/4b155496dc9c586bb543e20c6b80ab220cc4af68)
- [Fix infinite loop on event bus close and UTF-8 trim edge case](https://github.com/Dicklesworthstone/frankenterm/commit/5298c5a61231ba806bfe455d787a7d28fb5acf0b)
- [Scope watchdog: use live_count_by_tier to ignore closed scopes](https://github.com/Dicklesworthstone/frankenterm/commit/c95b61ce1f644aa5b9865bde54b5b843dc2981ad)
- [Prevent shutdown deadlock and expand signal handling](https://github.com/Dicklesworthstone/frankenterm/commit/7d8c0d2783c23557d92fe0f59b5faf109e435b7f)
- [Prevent subprocess bridge deadlock on daemonizing children](https://github.com/Dicklesworthstone/frankenterm/commit/95eb169fbe64426cdb4ccfad1c8a28ba96e4a940)
- [Prevent cursor-row panic when cursor is beyond pane bounds](https://github.com/Dicklesworthstone/frankenterm/commit/2722ce9a0b19b4841136d8b6fb135df99d9904fd)
- [Replace copy_nonoverlapping with copy in stream_decode (UB fix)](https://github.com/Dicklesworthstone/frankenterm/commit/a7b05007c83eb2bfaf69ecbc9288fc84b86bae18)
- [Scope tree: prevent infinite loop in descendants() on cyclic children](https://github.com/Dicklesworthstone/frankenterm/commit/7ea3c38f41ad3dc2c19c09784689a1482a389170)

### BREAKING: Remove Lua-based status update hook (2026-01-28)

Removed `update-status` Lua callback that fired at ~60Hz, causing continuous overhead. Alt-screen detection now via escape sequence parsing. Pane metadata via polling only when needed.

- [Remove Lua status update hook for performance](https://github.com/Dicklesworthstone/frankenterm/commit/ff24bb3f23be958f5fec88725ceb8aebb819d9aa)
- [Remove StatusUpdateReceived event variant](https://github.com/Dicklesworthstone/frankenterm/commit/ee66abdf20bfb90d8e0d99117fdcf5b19f8b66ac)

Migration: re-run `ft setup --wezterm` to update your `wezterm.lua`. The ft-managed block should no longer contain `wezterm.on('update-status'`, `wa_last_status_update`, or `WA_STATUS_UPDATE_INTERVAL_MS`. It should still contain `wezterm.on('user-var-changed'` for agent signaling.

---

## [0.1.0] -- 2026-01-25

> Initial release. ~469 commits across 2026-01-18 to 2026-01-25. Originally named `wezterm_automata` (`wa`), later renamed to `frankenterm` (`ft`).

### Core Platform

- **Rust workspace** with strict safety settings (`forbid(unsafe_code)`) and comprehensive lint configuration
  - [Configure workspace with strict safety and lints](https://github.com/Dicklesworthstone/frankenterm/commit/2df6aa108990b67fe9c775fc2a6ff12fb5549437)
- **WezTerm client and domain models** for pane discovery, fingerprinting, and lifecycle tracking
  - [Core library with WezTerm client and domain models](https://github.com/Dicklesworthstone/frankenterm/commit/0b3d4a9db84de4660caae6c0464093d5720adbbc)
- **CLI binary** with command structure and Robot Mode
  - [CLI binary with command structure and robot mode](https://github.com/Dicklesworthstone/frankenterm/commit/fffc22f85d5a1910805e52bc18df6e559822bf0c)

### Pattern Detection Engine

- Multi-agent pattern engine detecting rate limits, errors, prompts, and completions across Codex, Claude Code, and Gemini
- DetectionContext for agent filtering and deduplication
- Golden corpus regression harness for pattern validation
  - [DetectionContext for agent filtering and deduplication](https://github.com/Dicklesworthstone/frankenterm/commit/d40e9a22d7bdf6b25171a2ec15361ecdeee44e4a)
  - [Golden corpus regression harness](https://github.com/Dicklesworthstone/frankenterm/commit/ea97e8f3120eac4e34f5da1d967c57be9ae513a5)

### Robot Mode API

- JSON/TOON interface optimized for AI agent orchestration
- Consistent response schema: `ok`, `data`, `error`, `elapsed_ms`, `version`
- TOON output format for 40-60% token savings in AI-to-AI communication
- Robot commands: `state`, `get-text`, `wait-for`, `search`, `send`, `events`
  - [Extensive robot mode enhancements](https://github.com/Dicklesworthstone/frankenterm/commit/9a3066ed0f603e40eb75f82dd5e4a3674506bae0)
  - [TOON output for wa robot](https://github.com/Dicklesworthstone/frankenterm/commit/6d2a65fa8a887683e705ceb7143902bb43d297ee)
  - [Robot JSON schemas](https://github.com/Dicklesworthstone/frankenterm/commit/a7e91cdc177995a9562ce0844e0c919e13964160)

### Full-Text Search

- FTS5-backed search across all captured pane output with BM25 ranking and snippets
  - [FTS search API with BM25 ranking and snippets](https://github.com/Dicklesworthstone/frankenterm/commit/6cd591f978080995d0a0a4f32d838c16c14d2f71)

### Storage

- Comprehensive SQLite schema for events, patterns, panes, and captures
  - [Comprehensive SQLite schema](https://github.com/Dicklesworthstone/frankenterm/commit/7ea47c64adea16a63f4b8ef9da2a86354f46fdb1)

### Policy Engine

- ActionKind, PolicyDecision, authorize() API with capability gates and rate limiting
- PolicyGatedInjector for unified input injection with audit trail
  - [Policy model with authorize() API](https://github.com/Dicklesworthstone/frankenterm/commit/6f0ba75cf5de85d04cf6d1211346c7e2f1ee756a)
  - [PolicyGatedInjector for unified input injection](https://github.com/Dicklesworthstone/frankenterm/commit/e83554947dda13615bed63577a7bad33c85c0318)
  - [Audit trail emission for PolicyGatedInjector](https://github.com/Dicklesworthstone/frankenterm/commit/0b83d1f88a5dec44e1dd793eac252fe104f1b175)

### Delta Extraction

- Capture snapshot method and delta extraction with 4KB overlap matching
- Gap recording for sequence discontinuity detection
  - [Capture snapshot and delta extraction](https://github.com/Dicklesworthstone/frankenterm/commit/1a668cde22cebf7fd640d560a6018e8aab95f065)

### Ingestion & Observation

- Pane discovery with fingerprinting and lifecycle tracking
- OSC 133 semantic prompt marker parsing
- ObservationRuntime for passive pane monitoring
- Event bus with bounded channels and fanout
  - [Pane discovery with fingerprinting](https://github.com/Dicklesworthstone/frankenterm/commit/03d5c101970f7ab4a52839b40e68efd973f0f1f1)
  - [OSC 133 semantic prompt marker parsing](https://github.com/Dicklesworthstone/frankenterm/commit/b43bd1c9937d52bdc72808418ed208fa91f29a2b)
  - [ObservationRuntime for passive monitoring](https://github.com/Dicklesworthstone/frankenterm/commit/68c24fff31b9c34236b55e239479bf8f3b0b69bc)
  - [Event bus with bounded channels and fanout](https://github.com/Dicklesworthstone/frankenterm/commit/a1ca9a8a80986f35527f75f152ca6786a070bd11)

### Workflow Engine

- Workflow trait, WorkflowContext, per-pane workflow locks, and scheduling
- Workflow runner integrated into `ft watch --auto-handle`
- Resume incomplete workflows on startup
  - [Workflow trait and types](https://github.com/Dicklesworthstone/frankenterm/commit/f7708aa4cbf318ec2282d6963a41bad61518d7e2)
  - [Comprehensive workflow execution with runner](https://github.com/Dicklesworthstone/frankenterm/commit/a3b7244cd4588208b68f000d68b89e55614db717)
  - [Workflow runner into ft watch --auto-handle](https://github.com/Dicklesworthstone/frankenterm/commit/cae856b1f51fabb86f17c6dc21c45f5d47bbce75)
  - [Resume incomplete workflows on startup](https://github.com/Dicklesworthstone/frankenterm/commit/73a2df07e05f7adfa1e58d4cb2c6101393565ebb)

### IPC & Runtime

- Unix socket IPC for watcher daemon communication
- TailerSupervisor for adaptive pane polling
- Crash recovery module
- Hot-reload config broadcasting
  - [Unix socket IPC](https://github.com/Dicklesworthstone/frankenterm/commit/da3ba590bf48ef4d75b384caf818de6dbd551e61)
  - [TailerSupervisor for adaptive pane polling](https://github.com/Dicklesworthstone/frankenterm/commit/dc7aa9bdac59ba31858b165c869b174c71fa1ccb)
  - [Crash recovery module](https://github.com/Dicklesworthstone/frankenterm/commit/0cdef4d265e25ab65043bb2fc606a38ea768d4a6)
  - [Hot-reload config broadcasting](https://github.com/Dicklesworthstone/frankenterm/commit/f2b086e3aa6ba53407e0f472e9c347c6a75fc75b)

### Other Notable Additions

- MIT License ([40cc88fb](https://github.com/Dicklesworthstone/frankenterm/commit/40cc88fb23498d90997a101021db6810a368b078))
- Proactive recommendation engine ([c49f0856](https://github.com/Dicklesworthstone/frankenterm/commit/c49f0856339b36d1ee54d358d4fe4e10fb1c9150))
- Explainability system (`ft why`) ([ef4df7f4](https://github.com/Dicklesworthstone/frankenterm/commit/ef4df7f4096fd0479b7075b2e395e8db6ee1de82))
- TUI module with interactive views ([e7c5c11c](https://github.com/Dicklesworthstone/frankenterm/commit/e7c5c11c86c3e255ed746b076c9c556a839aa0f8))

### CI/CD & Testing

- GitHub Actions CI/CD workflows
- Criterion benchmarks for critical paths
- Daemon integration tests with synthetic deltas
- E2E test harness
  - [GitHub Actions CI/CD workflows](https://github.com/Dicklesworthstone/frankenterm/commit/31110d4277e13408d2eb099ff2d3372cbe725382)
  - [Criterion benchmarks](https://github.com/Dicklesworthstone/frankenterm/commit/ab5d821775acaed038897f7a326efda572749ef6)
  - [E2E test harness](https://github.com/Dicklesworthstone/frankenterm/commit/d4cd86177bc0bb7cf8ed1f3683f2f14053c36139)

---

## Tags & Releases

Current refs do not include a `v0.1.0` tag; the `0.1.0` sections above are changelog milestones reconstructed from history, not published GitHub Releases.

`Kind` distinguishes a published GitHub Release from a plain git tag. Every `v0.2.0`–`v0.14.1` tag below has a GitHub Release; `backup-before-rewrite` does not.

| Tag / Ref | Kind | Date | Points to | Description |
|-----------|------|------|-----------|-------------|
| [`v0.14.1`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.14.1) | Release | 2026-08-20 | tag `v0.14.1` | Pane-input argv privacy and release-contract repair |
| [`v0.14.0`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.14.0) | Release | 2026-08-20 | tag `v0.14.0` | Mux authority, scheduler admission, recorder truth, and protocol hardening |
| [`v0.13.0`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.13.0) | Release | 2026-07-28 | [`c366f3ac9`](https://github.com/Dicklesworthstone/frankenterm/commit/c366f3ac95a2a53d6d86e438f1432bdcf4981f26) | Test-suite honesty (ft-nam3s) + tx/capture/redaction hardening; full platform matrix returns |
| [`v0.12.0`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.12.0) | Release | 2026-06-29 | tag `v0.12.0` | asupersync 0.3.5 churn fix + window-maximize persistence |
| [`v0.11.0`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.11.0) | Release | 2026-06-27 | tag `v0.11.0` | Mux latency-hiding + mosh-grade predictive echo |
| [`v0.10.4`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.10.4) | Release | 2026-06-27 | tag `v0.10.4` | macOS GUI progressive-slowdown fix (shape-cache/atlas decoupling) |
| [`v0.10.3`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.10.3) | Release | 2026-06-25 | tag `v0.10.3` | macOS GUI GPU-atlas memory leak fix |
| [`v0.10.2`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.10.2) | Release | 2026-06-24 | tag `v0.10.2` | Round-9 convergence (quick_reject removal + WAL + scan_pipeline deletion) |
| [`v0.10.1`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.10.1) | Release | 2026-06-22 | tag `v0.10.1` | 0.10.1 point release |
| [`v0.10.0`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.10.0) | Release | 2026-06-21 | tag `v0.10.0` | Round-7 Alien Optimization Gauntlet |
| [`v0.9.0`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.9.0) | Release | 2026-06-20 | tag `v0.9.0` | Round-6 Alien Optimization Gauntlet |
| [`v0.8.0`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.8.0) | Release | 2026-06-20 | tag `v0.8.0` | Round-5 Alien Optimization Gauntlet |
| [`v0.7.0`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.7.0) | Release | 2026-06-19 | tag `v0.7.0` | 0.7.0 |
| [`v0.6.1`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.6.1) | Release | 2026-06-18 | tag `v0.6.1` | 0.6.1 |
| [`v0.6.0`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.6.0) | Release | 2026-06-15 | tag `v0.6.0` | 0.6.0 |
| [`v0.5.0`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.5.0) | Release | 2026-06-09 | tag `v0.5.0` | 0.5.0 |
| [`v0.4.0`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.4.0) | Release | 2026-06-08 | tag `v0.4.0` | 0.4.0 |
| [`v0.3.0`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.3.0) | Release | 2026-05-26 | tag `v0.3.0` | 0.3.0 |
| [`v0.2.0`](https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.2.0) | Release | 2026-05-21 | tag `v0.2.0` | First tagged release |
| `backup-before-rewrite` | Git tag (no GitHub Release) | 2026-02-17 | [`888c17d0`](https://github.com/Dicklesworthstone/frankenterm/commit/888c17d0da2564269df114e4c5d9ecfd8edf85c5) | Snapshot before the major WezTerm source import and codebase rewrite |

---

## Project Timeline

| Date | Milestone |
|------|-----------|
| 2026-01-18 | First commit. Workspace setup, core library, CLI binary, WezTerm client. |
| 2026-01-19 | Rapid feature buildout: FTS, events, patterns, policy, workflows, robot mode, IPC, CI. |
| 2026-01-25 | 0.1.0 feature set complete. Agent friendliness report, suggestions engine. |
| 2026-01-27 | Curl-bash installer, doctor command, action plans, risk scoring. |
| 2026-01-28 | Remove Lua status hook (BREAKING). SSH setup, chaos testing, backup export/import. |
| 2026-01-29 | Triage command, pane reservations, browser automation, CASS CLI, data export. |
| 2026-02-04 | Recording engine, secrets scanner, IPC auth, Prometheus metrics, MCP. |
| 2026-02-08 | FTUI migration complete. Undo/redo framework. Streaming mux subscription. |
| 2026-02-09 | Wire protocol aggregator. Distributed mode readiness. DirectMuxClient pool. |
| 2026-02-10 | **WezTerm source import.** Rename wa -> ft. Session persistence. |
| 2026-02-11 | PIE (Probabilistic Intelligence Engine): BOCPD, causal DAG, session DNA. |
| 2026-02-12 | Data structures library. Flight recorder. Runtime compat layer. |
| 2026-02-13 | WASM extension system. Resize subsystem. Config migration tool. |
| 2026-02-17 | `backup-before-rewrite` tag. Asupersync runtime abstraction begins. |
| 2026-02-19 | FrankenSearch: fusion backend, embedding daemon, WAL, 100+ proptests. |
| 2026-02-22 | Tool integration bridges (beads_rust, UBS, vibe_cockpit). Recorder backend-agnostic seam. |
| 2026-02-23 | Latency budget framework. ARS (Autonomous Reflex System). |
| 2026-03-01 | Dashboard aggregator. Cost tracker with budget alerts. |
| 2026-03-02 | **Native GUI terminal.** Mux server. FrankenTerm.app builds from source. |
| 2026-03-03 | Swarm orchestration runtime. Connector SDK. Native mux lifecycle. |
| 2026-03-10 | 21-subsystem policy engine. Forensic export pipeline. |
| 2026-03-11 | tokio->asupersync migration COMPLETE. Ops telemetry suite. |
| 2026-03-12 | Tiered scrollback. Fleet memory controller. 200-pane stress tests. |
| 2026-03-13 | Transaction execution engine. Input-to-display latency framework. |
| 2026-03-17 | Distributed checkpoint save/restore. Replay forensics with sensitivity tiers. |
| 2026-03-20 | CASS export feature. 92+ proptest serde roundtrip suites. |
| 2026-04-11 | **0.1.0** changelog baseline. First feature-complete baseline. |
| 2026-04-12 | Native asupersync cutover begins. tokio dual-runtime seams retired. |
| 2026-04-25 | Sub-crate carving wave begins (ft-y0loj.*). `frankenterm-core` shedding leaves. |
| 2026-05-01 | Doctrine epic closes (ft-i2eni): RuntimeProof sealed trait, asupersync_test! macro, cargo-deny tokio ban, vendored fork rename complete. |
| 2026-05-02 | Substrate audit waves (rubber-stamp `is_safe`, public-field bypass, NaN/sanitization) sweep across the codebase. |
| 2026-05-10 | Operating-envelope contract (ft-booek) + incident-bundle live collectors (ft-9sy9e family) land. |
| 2026-05-12 | Reality-check round 2 (ft-tf6g3) opens — final-mile convergence: attestation graph, renderer SLO suite, round-3 statistical elevations. |
| 2026-05-18 | HEAD/local `main`. 10,554 total commits; 3,558 since 2026-05-01 locally and 3,550 on `origin/main`. 77 workspace members. 512 top-level core modules. 1,481 tracked core test files, 56 fuzz target files, 265 E2E shell scripts, 628 tracked docs. Local `main` is ahead of `origin/main` by eight commits pending RCH proof/push coordination. |
| 2026-06-23 | **v0.10.2 — Alien Optimization Gauntlet round 9 (FULL convergence).** Removed the net-negative `quick_reject` Bloom prefilter (default-off, +35–43% per-delta detection / −22.76% of fleet detection self-time, byte-equivalent — ft-ui1xn); promoted the WAL skip-startup-checkpoint lever to default-on (+74% dirty-WAL startup, no regression — ft-yjihu.1); deleted the dead `scan_pipeline` module (4260 lines); caught one false-open (ft-uyt88 reader test hangs on macOS host — not the BufReader change). The 5-round optimization campaign (v0.7→v0.10.2) is declared **fully converged**. Ledgers: `docs/perf-ledger/round9-*`. |
| 2026-06-25 | **v0.10.3 — macOS GUI GPU-memory leak fix.** Fixed a progressive GPU-atlas leak that made the GUI lag worse over hours until restart: the thread-local glyph-run shaping cache held `Rc<CachedGlyph>` (which owns the ~49 MB atlas texture), so each atlas recreation leaked the old generation (~691 MB IOSurface / ~14 generations observed in the field). Fix = lifetime decoupling — the cache now stores only the atlas-invariant shaping (positions) and re-attaches live glyphs on hit: leak-free by construction, more correct (always renders the live atlas), and faster than clearing. Hardened with a `cache_gpu_handle` CI lint that makes "a global cache owns a GPU handle" a build failure, a 64-generation churn behavior-proof test, and `docs/render/gpu-cache-lifetime-invariant.md`. Stopgap on older builds: `FT_DISABLE_GLYPH_RUN_INTERNING=1`. |
| 2026-06-25 | **v0.10.4 — macOS GUI progressive-slowdown fix (the real one).** Fixed the render-loop CPU climb (~30%→70% over ~40 min of a long swarm session) that made the GUI laggier until restart. Root cause: the glyph atlas is a bump-allocator that reclaims space only by a full rebuild, and every overflow **wiped the shape cache**, forcing a full-screen **HarfBuzz re-shape**; as cumulative glyph diversity grew, these re-shapes became frequent → render CPU climbed. (The ~1 GB of GPU surfaces was a stable red herring — the atlas pinned at its 256 MiB cap — which is why v0.10.3's GPU-memory changes didn't help.) Fix: **decouple the shape cache from the atlas** — cache the atlas-invariant HarfBuzz output + a generation tag and re-resolve glyph sprites cheaply on rebuild instead of re-shaping (the same lifetime-decoupling as the v0.10.3 interner fix, applied to the cache that dominates). Verified: ~56% fewer HarfBuzz calls per atlas rebuild, ~31% lower render CPU on a throttled repro, shape tests green; re-resolve is byte-identical to a fresh shape. |
| 2026-06-29 | **v0.12.0 GitHub Release.** asupersync 0.3.5 churn fix + window-maximize persistence. |
| 2026-07-28 | **v0.13.0 GitHub Release.** Test-suite honesty campaign (ft-nam3s) + tx/capture/redaction hardening; full platform matrix returns. |
| 2026-08-19 | HEAD `bb6809b3d`. Janitor docs-reorg: root ELF/scratch removed; plans and wizard cluster moved into `docs/planning/`. Mux exact-owner/census campaign continues on `main` (1,271 non-merge commits since v0.13.0). |

---

<!-- Links -->
[Unreleased]: https://github.com/Dicklesworthstone/frankenterm/compare/v0.15.1...main
[0.13.0]: https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.13.0
[0.12.0]: https://github.com/Dicklesworthstone/frankenterm/releases/tag/v0.12.0
[0.1.0]: https://github.com/Dicklesworthstone/frankenterm/commits/main/?after=backup-before-rewrite
