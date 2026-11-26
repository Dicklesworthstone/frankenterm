# Resumable terminal-output SSE

`GET /stream/deltas` streams redacted, retained output segments as
`ft.stream.v1` frames. This endpoint remains subject to the existing web
server's binding and access restrictions. The resume cursor is not an
authentication credential.

## Starting and resuming

A request without a resume cursor is a fresh live subscription. After subscribing
to notifications, the server reads the greatest currently retained segment ID
in the requested pane scope, or zero for an empty scope, and starts after that
boundary. Capture timestamps do not determine arrival: output committed after
the boundary remains eligible even when the clock moves backward. To explicitly
replay retained history, use `?since_id=0`. To resume a previous connection,
send its complete `delta-v2` token in `Last-Event-ID` or `?cursor=<token>`.
The token identifies the capture epoch, pane selection and persisted
`data.segment_id`, not the frame's `seq` or the pane-local `data.seq`.
`pane_id`, when supplied, must be a nonnegative unsigned integer; invalid text
is rejected rather than interpreted as an all-pane subscription.

The initial `ready` checkpoint and each `delta` message carry an opaque,
versioned SSE id:

- All panes: `delta-v2:<epoch>:all:<segment_id>`.
- One pane: `delta-v2:<epoch>:pane:<pane_id>:<segment_id>`.

The epoch is an opaque, durable 32-character lowercase hexadecimal identifier.
Segment IDs advance monotonically within that epoch, including after retention
deletes the highest IDs or the entire output table. The allocation watermark is
independent of the currently retained tail and is not exposed by scoped cursors.

For example, a pane-7 message for persisted segment 9001 has:

```text
event: delta
id: delta-v2:0123456789abcdef0123456789abcdef:pane:7:9001
data: {"schema":"ft.stream.v1","stream":"deltas","kind":"delta","seq":2,"ts_ms":123,"data":{"segment_id":9001,"pane_id":7,"seq":31,"captured_at":100,"content_len":5,"content":"hello"}}

```

A reconnecting EventSource supplies that id in `Last-Event-ID`. A nonempty,
valid header takes precedence over the original URL's `cursor` and `since_id`, so an
unchanged bootstrap URL cannot repeatedly replay already-received output.
An absent or empty header falls back to `cursor`, then `since_id`. A malformed
nonempty header or explicit cursor is rejected; it never falls back to another
parameter or adopts the current database epoch. Both fresh and resumed
connections scan strictly after their admitted segment ID; older capture
timestamps remain eligible. Scanning starts immediately, without needing another
EventBus notification. Failure to read a fresh boundary returns HTTP 503 with
`storage_error` before opening the stream.

A cursor is bound to the same pane selection. Reusing a pane-7 cursor for pane
8 or for all panes fails with HTTP 400 and `delta_cursor_scope_mismatch`.
Starting a deliberately different selection requires a fresh subscription or
an explicit numeric `since_id` bootstrap appropriate to that selection. Numeric
bootstraps deliberately query the current capture database; they do not establish
continuity with an earlier database. They remain available for explicit history
and finite capture queries. A client must retain the resulting full ready cursor
for any later reconnect.
Malformed, overflowing, or non-UTF-8 cursors fail with HTTP 400 and
`invalid_delta_cursor`; raw cursor input is not echoed in errors. Decimal
segment ids are canonical unsigned decimal strings bounded by the signed 64-bit
storage id range. Cursor tokens are bounded to 128 bytes; embedded CR, LF and NUL
are rejected before trimming HTTP whitespace.

## Legacy clients and control frames

Earlier connection-local numeric IDs and `delta-v1` IDs do not identify a
capture epoch. Both are rejected as reconnect tokens. There is no automatic
conversion to the current epoch; migrating clients must choose an explicit
fresh or retained-history view. Old server and browser packages must be updated
together for the versioned protocol.

The JSON envelope's `seq` remains per connection. `ready` sets the SSE `id` to
the admitted boundary, including zero for an empty fresh scope or explicit
history replay. Its payload reports the same `cursor` and `resume_after_id`,
along with `cursor_format: delta-v2` and `recovery: retained_segments_only`. This checkpoint
allows a connection that has received no output yet to recover output committed
while disconnected; it does not acknowledge any transcript delivery or complete
history catch-up. Subsequent `delta` frames advance the cursor. `gap`, `lag`,
`error`, `cursor_discontinuity`, and keepalive comments carry no id field; they never send an empty `id:`
that would clear the browser's cursor.

## Delivery and failure boundaries

Storage scans are page-bounded and yield between configured scan windows.
There is no total-row replay truncation. Frames wait for bounded channel
capacity under the connection context; the internal cursor advances only
after enqueue succeeds. Enqueue is not an acknowledgement that the client
received or processed the bytes. Reconnect always uses the client's supplied
last-received cursor, so unread buffered frames remain eligible for replay.
Application-level consumers that checkpoint before processing still need their
own acknowledgement and deduplication policy.

Query failures and invalid page ordering/filter results emit a finite error
class and end the stream without assigning a newer cursor. Periodic catch-up
also runs when unrelated-pane traffic continues, repairing missed storage
notifications. The response owns the producer, including capacity and pacing
waits; dropping the response or signalling server shutdown disposes of it.

Each bounded storage page includes its epoch and allocation watermark from the
same SQLite statement, even when no output matches. Admission and every page
refuse a different epoch or a cursor ahead of the durable allocation watermark.
An active connection also refuses a watermark lower than one it has already
observed. These failures emit one terminal `cursor_discontinuity` frame and end
the stream without assigning a new SSE ID or delivering rows from that page.
An admission failure precedes `ready`, allowing EventSource clients to display
the terminal reason without reading an HTTP error body.

The diagnostic payload contains `code: delta_cursor_discontinuity`,
`terminal: true`, the original `resume_cursor`, and one finite `reason`:
`cursor_epoch_mismatch`, `cursor_ahead_of_high_water`, or `high_water_regressed`.
It supplies no replacement cursor and exposes no other pane's allocation count.
Clients must keep their accepted cursor and require an explicit new view to
adopt a different capture history.

Recovery covers appended segments that remain in the same capture database.
It does not recreate retention-deleted output, replay historical capture-gap
notifications, or replay in-place
changes to previously delivered rows. Sparse ids alone do not prove deletion,
because filtered panes also create holes. Epoch changes, cursor-ahead positions
and observed watermark regressions are detectable discontinuities; this is not
a complete output-retention ledger. Copying or restoring a whole database also
copies its epoch. A same-epoch restore that has already reached the cursor's ID,
without a lower watermark observed by an active connection, cannot be identified
from this token alone. Restore tooling must choose a new capture identity if it
needs that stronger guarantee. A fresh connection that fails before receiving `ready` still has no
stored SSE checkpoint unless the caller supplied an explicit bootstrap boundary.
These limits are not a claim of unconditional lossless terminal capture.

## Regression coverage

`web::sse::delta::tests` contains source-level regressions for bounded-page
catch-up, full-channel delivery, shutdown, query failures, malformed pages,
redaction, cursor parsing and scope, ready checkpoints, historical resume,
epoch changes on populated and empty pages, watermark rollback and disconnect
with unread queued frames. The real HTTP/SQLite regression in
`tests/web.rs` also covers empty and populated fresh tails, ready-only reconnect,
backward capture timestamps and repair without a bus notification. Tests were
added with this implementation
but were not executed in the authoring environment: Rust tools and the required
RCH execution capability were unavailable. Compile, focused runtime tests,
Clippy, formatting, and HTTP/SQLite end-to-end verification remain required;
source presence is not runtime proof.

## Browser pane-output inspection

The browser package's source now connects this endpoint to a read-only output
panel. Select a pane or detection row and press Enter or **Inspect selected
pane**, or enter an exact decimal pane ID in the toolbar. The output remains
bound to that pane until another explicit Open action; moving the fleet
selection does not retarget it. Tab leaves the fleet canvas for native controls;
1 and 2 still select the pane and detection views.

Opening starts with new output. **Load retained history** explicitly replaces
the view and replays from `since_id=0`; large histories can take time to drain.
The panel does not claim that a ready frame means history catch-up is complete.
**Pause** closes the subscription while retaining the received cursor; **Resume**
replays retained rows after it. Page suspension also closes transport, but
resuming the page does not override an explicit pause. A capture discontinuity
preserves accepted text and closes transport and timers. Pause/resume, page
visibility and stale callbacks cannot restart it; **Open** or **Load retained
history** explicitly selects the current history. Closing the panel clears
its captured text and cursor. Neither is saved to local storage for reloads.

**Find text** performs literal, case-sensitive search within the currently
retained transcript, with next/previous wraparound. Searching pauses the stream
so matches do not move. The native read-only text area supports selection and
copy; no input, paste, click, or terminal control sequence is sent to a pane.
Captured HTML and escape sequences are text, not rendered markup or commands.

The default local view retains at most 262,144 UTF-16 code units and 2,048
nonempty chunks, discarding older text with a visible warning. Unicode-tail
truncation never retains half a surrogate pair. Individual dispatched messages
are limited to 1,048,576 code units before JSON decoding. This admission limit
is after EventSource dispatch and does not bound the browser's internal network
parser buffers or establish a measured process-memory envelope. Presentation
is coalesced into the existing animation loop; transcript text is not a live
region that announces every output chunk.

Both the scoped SSE cursor and JSON identities must agree with the selected
pane, preserving the full u64/i64 ranges without JavaScript Number rounding.
Malformed frames do not advance the received cursor. A validated ready checkpoint
is retained across quiet reconnect, pause and page suspension without claiming
that output was delivered. Its scoped SSE ID, payload cursor and exact numeric
boundary must agree; reconnect may not jump past an already accepted boundary.
Owned replacement connections carry the complete token in `?cursor`, preserving
its epoch. Ready, delta and inherited control IDs cannot switch that epoch.
Gap/lag notifications, retention truncation, server failures, and reconnection
before the first checkpoint remain visible. The database-identity and
retained-history limits above apply.

Historical search results include a `capture_cursor` bound to the epoch read
with the returned capture row and snippet. It names that pane and the segment's
immediate predecessor. The browser carries this as `captureCursor`, requires it
before replacing the current view, and opens the finite read with `?cursor`.
The first ready checkpoint must match the selected epoch and exact predecessor;
the delivered segment must still match its ID, pane, capture time and length.
Missing, malformed or inconsistent search cursors have no numeric fallback.
A result retained from a different database cannot silently open an identically
numbered capture in today's database. Search again after a terminal identity
failure to choose a current result. The same-epoch copy/restore limitation
above still applies.

Build and deploy a fresh browser package through the approved build lane;
updating only HTML is insufficient because the viewer adds the `selectedPaneId`
WASM export. The package builder includes `pane-output.mjs` in its hashed asset
set. `node --test tests/browser/pane-output.test.mjs` exercises the production
controller and panel with controlled transport/DOM/model dependencies. These
tests do not establish a compiled WASM binding, native rendering, browser
accessibility qualification, or a real FrankenTerm HTTP/SQLite end-to-end pass.
