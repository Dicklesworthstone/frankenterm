# Tab-order authority and continuity contract

Status: accepted design contract for
`ft-interactive-swarm-product-convergence-7xqz4.8.10.1`.

This contract defines identity, ownership, ordering, active-tab, conflict, and
restart semantics for durable tab order. It is intentionally stricter than the
current implementation. The protocol, server, client, storage, GUI, checkpoint,
resilience, and native-proof work remains in sibling beads under
`ft-interactive-swarm-product-convergence-7xqz4.8.10`.

## 1. Decision

There are two, and only two, order authorities:

1. A remote mux server is authoritative for the ordered tabs and active tab of
   one exact remote mux window.
2. A client layout overlay is authoritative for the composition of a GUI
   window that mixes local tabs, different remote mux windows, or different
   mux sessions/domains.

A local GUI reorder may be displayed optimistically, but it becomes
authoritative only after the appropriate authority commits it:

- a revision-checked server commit for a pure remote window; or
- a crash-consistent local overlay commit for a mixed window.

No other state is order authority. In particular, process-local numeric IDs,
hash-map iteration, flat `ListPanesResponse.tabs` encounter order, numeric
sorting, timestamps, titles, domain display names, pane sets, and focus
notifications are not durable tab-order authority.

## 2. Current implementation truth and negative-evidence ledger

| Surface | Current truth | Contract consequence |
| --- | --- | --- |
| `mux::Window` | `tabs: Vec<Arc<Tab>>` and `active: usize` preserve order and active selection inside one live mux process. | This is the server-side state to publish and revision, not a durable identity by itself. |
| `Window::{push, insert, remove_by_idx, remove_tab_if_same, save_and_then_set_active}` | These mutate the live vector/active index and emit broad invalidation/focus effects, but carry no incarnation, order revision, idempotency, or atomic reorder event. | They remain low-level live-object primitives. Authoritative server transitions must wrap them in one validated revisioned commit; mixed-overlay intent must not be mistaken for a server commit. |
| `Mux::{add_tab_to_window, remove_tab, remove_tab_local_only, move_tab_between_windows}` | These attach, detach, and reparent exact live `Arc<Tab>` objects. Existing move/reorder publication is not one atomic versioned order transition. | Pure-window membership/order changes belong to server authority. Local mirror cleanup and mixed composition remain client projection/overlay operations and cannot rewrite remote order. |
| `TermWindow::move_tab` | Removes and inserts only in the local mirrored `mux::Window`; it sends no reorder RPC. | The operation is optimistic projection only for a remote window. |
| GUI drag/drop and window-unify paths | They can compose, move, or discard local mirrors from more than one remote source. | One post-operation exact-identity classification routes the intent to server CAS only when pure; every other composition routes to the client overlay. |
| Mux-server `ListPanes` handler | Iterates mux windows and snapshots tab vectors into flat trees/titles without one topology revision or active-tab identity. | It is neither a coherent causal fence nor a durable ordering snapshot. |
| `ListPanesResponse` | Flattens tab trees and titles; window iteration comes from a hash map; no explicit ordered-window or active-tab record exists. | A client must not infer durable order or active identity from the flat vectors. |
| `ClientDomain::process_pane_list` | Appends missing local mirrors in encounter order and does not reorder existing mirrors. | Reconnect/app-reopen order is currently best effort and may appear random. |
| `PaneFocused` / `SetFocusedPane` | Indirectly reconciles the pane and containing active tab. | Useful live focus signaling, but not an authoritative order snapshot or reconnect bootstrap. |
| `session_topology::TopologySnapshot::from_panes` | Groups in hash maps and sorts window and tab IDs numerically. | Schema v1 is deterministic but not user-order preserving. |
| Session/checkpoint restore | Recreates layout using schema-v1 numeric IDs and indices rather than a validated live incarnation mapping. | It may restore approximate layout, but cannot claim live order/active continuity until the checkpoint bead migrates the schema and proves identity mapping. |
| `mux::unify::TabIdentity` | Uses process-local domain ID plus a sorted remote-pane set as a duplicate-mirror heuristic. | It is not a durable session/tab identity and must never key persistence or reorder CAS. |
| Client domain config | Names and transport locators identify how to try a connection. | They are routing hints, not proof that a newly reached server is the prior mux session. |
| Numeric ID allocators | Tab/window/domain constructors still use a legacy saturating allocator pending `ft-interactive-systems-performance-4tenz.5.5.13`. | Reuse or terminal-value duplication must fail closed; order persistence cannot paper over it. |

## 3. Identity vocabulary

The names below are semantic types. The protocol bead owns their final wire
representation.

### 3.1 `DomainBindingId`

A client-owned, random 128-bit identifier for one configured connection
binding. It is persisted in project-owned state and is stable across GUI
process restarts. It contains no credentials.

The binding ID may be looked up using a canonical, privacy-safe fingerprint of
the non-secret transport target. The display name is not part of authority. If
the target changes and there is no explicit migration, a new binding ID is
created.

`DomainBindingId` answers “which client connection binding is this?” It does
not prove which server session was reached.

### 3.2 `MuxSessionIncarnation`

A server-owned, unpredictable 128-bit identifier for one live mux-session
incarnation. It is returned by the negotiated handshake/topology snapshot and
is stable across client disconnect/reconnect and GUI close/reopen while that
mux session remains alive.

A mux-server process restart creates a new incarnation. Reusing an incarnation
after restart is permitted only if one atomic durable restore also restores
every scoped window/tab identity, tombstone, idempotency record required for
replay, and nonwrapping revision. FrankenTerm does not currently meet that
precondition, so current server restart means a new incarnation.

### 3.3 `TopologyStreamId`

A server-owned, unpredictable 128-bit identifier for one connection-generation
subscription to the session-global topology revision stream. It rotates on
server incarnation change, reconnect bootstrap, or any loss-terminal topology
queue transition. Rotating one connection's stream does not invalidate another
connection's subscription. A full snapshot establishes a stream; subsequent
order/topology events on that connection must carry that same stream ID and a
contiguous topology revision.

The stream ID is a causal fence, not durable tab identity.

### 3.4 Stable remote keys

```text
RemoteWindowKey = (MuxSessionIncarnation, RemoteWindowId)
RemoteTabKey    = (MuxSessionIncarnation, RemoteTabId)
RemotePlacement = (DomainBindingId, RemoteWindowKey, RemoteTabKey)
```

`RemoteTabKey` deliberately excludes the window ID so the same tab retains its
identity during an atomic move between windows. `RemotePlacement` records its
current parent.

Numeric IDs must not be reused within one `MuxSessionIncarnation`. Exhaustion
is terminal for further allocation; it must not wrap, saturate, reset, or
silently reuse a tombstoned ID.

### 3.5 Stable local keys

A local tab can occupy a durable mixed overlay only if its owning local/session
runtime supplies an equivalent incarnation-scoped stable key. A process-local
`TabId` alone does not qualify. If a local tab cannot be reattached or restored
with stable identity, the overlay may retain a missing-slot diagnostic but
must not recreate or match it by title, cwd, command, timestamp, or numeric ID.

## 4. Pure versus mixed ownership

A GUI window is a **pure remote projection** only when all of the following
hold at one validated topology revision:

- every tab maps to the same `DomainBindingId`;
- every tab maps to the same `MuxSessionIncarnation`;
- every tab maps to the same `RemoteWindowKey`;
- the vector is the complete live tab membership of that remote window; and
- there are no local, unknown, unavailable, or foreign slots.

The remote mux server owns both order and active tab for a pure projection.
The client must reconcile to the server snapshot without recreating live
`Arc<Tab>` or `Arc<dyn Pane>` objects.

Every other GUI window is **mixed**. The client overlay owns only:

- the ordered vector of stable local/remote slot keys;
- the active slot key;
- client workspace/window association; and
- a local nonwrapping overlay revision.

A mixed overlay cannot reorder a remote server window. Selecting a remote slot
may still publish focus to that slot's server, but that does not transfer
cross-domain composition authority to the server.

On passive reconnect or startup, an authority transition from mixed to pure
always chooses server order. A user action that removes the final foreign slot
may explicitly submit the surviving exact full permutation as a normal
revision-checked server reorder. There is no implicit write on an authority
transition.

## 5. Revision and causal contract

Each server snapshot contains:

```text
MuxSessionIncarnation
TopologyStreamId
TopologyRevision
for each window:
    RemoteWindowKey
    WindowOrderRevision
    ordered RemoteTabKey vector
    active RemoteTabKey or None
```

- `TopologyRevision` is session-global and increments once for every committed
  topology transition. Each connection's lossless stream carries the same
  committed revisions under its own `TopologyStreamId`.
- `WindowOrderRevision` is per-window and increments once when that window's
  membership, order, or active tab changes.
- A transition affecting two windows increments both window revisions and the
  topology revision in one commit.
- Both counters use checked addition. Exhaustion rejects the transition before
  mutation and emits a typed terminal/exhausted outcome.
- An order request compares the exact session/window identity and
  `WindowOrderRevision`. The global topology revision binds the response/event
  into the one topology stream without making unrelated-window activity cause
  spurious CAS conflicts.

This is one causal authority: window revisions are subordinate version stamps
published only by commits in the topology stream. They are not independent
clocks reconstructed by clients.

## 6. Reorder request and idempotency contract

A pure-window reorder intent contains:

```text
protocol/schema version
DomainBindingId (routing and audit context)
MuxSessionIncarnation
RemoteWindowKey
expected WindowOrderRevision
exact desired permutation of every current RemoteTabKey
desired active RemoteTabKey or None
MutationId
payload digest
```

`MutationId` is unique within a random client mutation namespace and a
nonwrapping client sequence. The server retains a bounded replay ledger keyed
by `(MuxSessionIncarnation, MutationId)`. The client pins the exact base
window vector, active identity, and revision associated with an outstanding
intent; that base snapshot is client-side rebase evidence, not a second
authority and need not be echoed over the wire.

Validation order is:

1. negotiated capability and size/count bounds;
2. exact session and window incarnation;
3. idempotency lookup;
4. duplicate/foreign/missing tab and active-membership validation;
5. expected window revision;
6. counter capacity; and
7. one atomic commit.

The typed result is one of:

- `Applied`: exact committed vector, active identity, and new revisions;
- `Replay`: the byte-equivalent prior terminal result for the same mutation and
  payload;
- `Conflict`: no mutation, with the current authoritative window snapshot;
- `StaleIncarnation`: no mutation and no numeric-ID fallback;
- `Malformed`: no mutation, including MutationId reuse with a different
  payload; or
- `Exhausted`: no mutation because an identity/revision namespace cannot
  advance.

If a replay-ledger entry has expired, a retried request is evaluated normally.
Its old expected revision therefore conflicts rather than applying twice.
The first implementation expires the oldest terminal receipt by commit/decision
insertion order before inserting receipt 4,097. Pending operations are
separately bounded and are never silently evicted while in flight. The client
uses the same oldest-terminal-first rule at its 1,024-receipt bound. There is
no silent last-write-wins path.

## 7. Transition semantics

| Transition | Authority and deterministic result |
| --- | --- |
| Initial attach | A negotiated full server snapshot establishes pure-window order and active identity. Flat legacy encounter order is not promoted to authority. |
| Pure-window user reorder | The GUI may display the intent immediately, then submits one bounded CAS request. `Applied` keeps it; any other result reconciles to returned/server truth. |
| Pure-window active-tab change | The server commits the active `RemoteTabKey`, increments the window/topology revisions, and publishes the result. Reordering preserves active identity, not its old index. |
| Concurrent reorders | The first matching expected revision commits. Every later request for that revision receives `Conflict`; it cannot overwrite the winner. |
| Duplicate delivery/reconnect retry | The same MutationId and payload returns `Replay`; the transition is not applied twice. |
| Stale client | A stale session/window identity returns `StaleIncarnation`; a stale window revision returns `Conflict`. |
| New tab | Server creation appends to the authoritative window unless the same atomic create/move transaction carries an explicit validated insertion position. Clients merging older intent append unseen tabs in their current authoritative server order. |
| Close non-active tab | Remove it, retain the active identity, preserve survivor relative order, tombstone the closed key, and advance revisions once. |
| Close active tab | Server authority applies its explicit close-focus policy. If the configured last-active policy names a still-live stable tab, choose it; otherwise choose the right neighbor at the removed position, then the left neighbor, then `None`. The server publishes the chosen identity. |
| Move between windows | One atomic server transaction removes the stable `RemoteTabKey` from its source, inserts it at the validated destination position, updates both active identities deterministically, and advances both window revisions plus one topology revision. Destination active stays unchanged unless it was empty; source active follows the close-active rule. |
| Numeric ID reuse | Reuse inside one session incarnation is malformed/terminal. A new session incarnation may use the same number but never matches the prior key. |
| GUI close/reopen | If the remote mux session stayed alive, its pure order/active state is returned by the next full snapshot. Mixed composition comes only from the validated client overlay. |
| Client reconnect | A new `TopologyStreamId` and full snapshot bootstrap the same `MuxSessionIncarnation`; no old stream event may apply afterward. |
| Server restart | A new `MuxSessionIncarnation` invalidates old live keys. Checkpoint restore may recreate layout under new identities but is not live reattachment. |
| Corrupt/future/oversized persistence | Preserve the live mux untouched, retain recoverable evidence, and ignore/quarantine the record through the resilience policy. Never partially apply it. |
| Legacy peer | Mark order durability unsupported. Preserve best-effort encounter order only for that live projection; do not persist or publish it as authoritative. |

## 8. Conflict rebase policy

An automatic rebase is allowed at most once. The client compares the common
surviving tabs in the intent's pinned **base vector** with those tabs in the
returned server snapshot. If their relative order is unchanged, the conflict
contains membership/active-state movement rather than evidence of a concurrent
reorder. The client may then filter closed tabs from the desired vector, append
new tabs in their current server order, choose the desired active key if it is
still live (otherwise the returned server active key), and submit one new CAS
against the returned revision.

If common-tab relative order between the pinned base and server truth differs,
another reorder may have committed: server truth wins immediately and the
conflict is surfaced by the resilience/UX bead. A second conflict also ends
automatic retry. Comparing the desired vector directly with server truth is
incorrect because it would misclassify the user's own intended reorder as a
concurrent writer.

## 9. Mixed-overlay reconciliation

For a validated mixed overlay and a live set of stable slot keys, a non-empty
persisted vector must name an active member. Missing active identity is corrupt
state, not an invitation to guess. After that validation:

1. retain persisted keys that are live, in persisted relative order;
2. retain unavailable-domain placeholders only under the bounded policy owned
   by the storage/resilience beads;
3. append previously unseen live keys in their source-authoritative order,
   grouped by their current GUI insertion event rather than numeric ID;
4. use the persisted active key if it is live;
5. otherwise choose the first live key to its right in the persisted vector,
   then the first live key to its left, then the first appended live key, then
   `None`; and
6. never match a missing key using title, pane set, cwd, command, timestamp, or
   a reused numeric ID.

The overlay's local revision and writes are independent of server topology
revision, but every remote slot is still incarnation-scoped. Overlay
persistence is asynchronous and coalesced; no keypress, parser, render,
resize, or present path performs synchronous disk or network work.

## 10. Persistence and restart boundaries

| Boundary | Pure remote window | Mixed GUI composition |
| --- | --- | --- |
| GUI window close/open, same process | Server snapshot restores exact order/active tab. | Live overlay projection restores exact stable slots. |
| GUI process restart, server remains alive | Server incarnation and snapshot restore exact order/active tab. | Crash-consistent client overlay restores validated slots. |
| Connection loss/reconnect to same server session | Full snapshot on a new stream restores exact state. | Overlay is reconciled against newly validated remote keys. |
| Mux-server process restart today | Unsupported as same live identity; new incarnation. | Old remote slots become stale/missing and cannot target replacements. |
| Explicit checkpoint layout restore | Recreates topology under new live identities unless a future atomic identity-preserving restore satisfies section 3.2. | Checkpoint/overlay migration maps only through explicit provenance, never numeric coincidence. |
| Domain rename, same binding record | Binding ID may remain; server incarnation still proves the live session. | Overlay remains eligible after exact server identity validation. |
| Endpoint/config target change | New binding unless explicitly migrated. | Old slots remain non-authoritative evidence and cannot mutate the new target. |

## 11. Validation and resource bounds

The first protocol implementation uses these hard ceilings:

- 4,096 ordered windows per snapshot;
- 4,096 tabs per window;
- 16,384 tabs total per snapshot;
- 4 MiB for the encoded ordered-window section;
- 512 KiB for one reorder request;
- 4,096 retained idempotency receipts per server session; and
- 1,024 retained receipts per client mutation namespace.

Implementations may negotiate lower limits but may not silently exceed these
ceilings. Counts and encoded bytes are validated before allocation or
mutation. Unknown/duplicate windows or tabs, a tab in multiple windows, an
active tab outside its window, impossible `None` active state for a non-empty
window, and any nonwrapping counter exhaustion fail before mutation.

Listing and reconciliation are `O(windows + tabs + panes)`. A pure reorder is
`O(tabs in the affected window)` and allocates at most one bounded validation
set plus the desired vector. There is no per-frame polling and no per-cell
identity validation.

Persisted and wire records contain IDs, revisions, schema/capability versions,
and finite reason codes only. They contain no titles, terminal contents,
commands, cwd, environment, credentials, socket paths, hostnames, or usernames.

## 12. Legacy and upgrade behavior

Durable order support is a negotiated capability tied to a codec version that
defines the complete snapshot and reorder result. An additive serde default or
the successful decoding of old `ListPanesResponse` is not evidence that the
peer supports this contract.

When capability is absent:

- the server/client continue legacy live behavior;
- order status is explicitly `legacy_best_effort`;
- no reorder CAS is sent;
- no pure-window order record is persisted as authoritative;
- active tab may continue to converge through `PaneFocused`, without a restart
  durability claim; and
- upgrading requires a new authoritative full snapshot before persistence is
  enabled.

Downgrading forgets no evidence automatically and performs no destructive
state cleanup.

## 13. Executable model cases

The deterministic contract model in `frankenterm/mux/src/window.rs` covers:

- initial attach and explicit active identity;
- applied reorder;
- concurrent reorder conflict;
- duplicate/idempotent reconnect replay;
- stale revision and stale incarnation;
- server restart;
- new-tab append;
- close and active fallback;
- cross-window move;
- tombstone/numeric-ID reuse;
- malformed permutations, invalid active identity, revision exhaustion, and
  window/per-window/aggregate count and index bounds;
- bounded receipt eviction followed by normal CAS validation;
- one-shot membership-only conflict rebase versus concurrent reorder;
- pure versus mixed authority;
- mixed-overlay reconciliation;
- corrupt persisted vectors; and
- legacy peers.

The model is an oracle, not a wire compatibility claim. The protocol/server/
client/storage implementations must test their real types against the same
transition table.

## 14. Implementation ownership and nonclaims

- `.8.10.2` owns negotiated wire types, capability/version changes, topology
  stream integration, codec properties, and malformed/boundary tests.
- `.8.10.3` owns server revision state, CAS, tombstones, replay receipts, and
  durability.
- `.8.10.4` owns exact-object client reconciliation.
- `.8.10.5` owns crash-consistent mixed overlays.
- `.8.10.6` owns optimistic GUI publication and focus-safe reconciliation.
- `.8.10.7` owns topology snapshot schema migration and restore.
- `.8.10.8` owns diagnostics, quotas, backoff, conflict UX, and recovery.
- `.8.10.9` owns exact-bundle native close/reopen evidence in an authorized
  environment.
- `ft-interactive-systems-performance-4tenz.5.5.14.1.2.3.2` owns the topology
  stream/fence prerequisite shared by the order protocol.
- `ft-interactive-systems-performance-4tenz.5.5.13` owns fail-closed migration
  of the current saturating numeric ID allocators.

This contract does not claim that current `ListPanesResponse`, session topology
schema v1, `PaneFocused`, or the GUI's local `move_tab` already preserves order
across reconnect or app reopen. It also does not authorize automated agents to
launch, close, inspect, or otherwise commandeer a user's live FrankenTerm GUI.
