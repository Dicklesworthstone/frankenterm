# Resumable terminal-output SSE

`GET /stream/deltas` streams redacted, retained output segments as
`ft.stream.v1` frames. This endpoint remains subject to the existing web
server's binding and access restrictions. The resume cursor is not an
authentication credential.

## Starting and resuming

A request without a resume cursor is a fresh live subscription. It retains the
existing capture-time cutoff at connection admission rather than replaying all
history. To explicitly replay retained history, use `?since_id=0`; to resume
from a known segment, use `?since_id=<last_received_segment_id>`. The id is the
persisted `data.segment_id`, not the frame's `seq` or the pane-local `data.seq`.
`pane_id`, when supplied, must be a nonnegative unsigned integer; invalid text
is rejected rather than interpreted as an all-pane subscription.

Each `delta` message now carries an opaque, versioned SSE id:

- All panes: `delta-v1:all:<segment_id>`.
- One pane: `delta-v1:pane:<pane_id>:<segment_id>`.

For example, a pane-7 message for persisted segment 9001 has:

```text
event: delta
id: delta-v1:pane:7:9001
data: {"schema":"ft.stream.v1","stream":"deltas","kind":"delta","seq":2,"ts_ms":123,"data":{"segment_id":9001,"pane_id":7,"seq":31,"captured_at":100,"content_len":5,"content":"hello"}}

```

A reconnecting EventSource supplies that id in `Last-Event-ID`. A nonempty,
valid header takes precedence over the original URL's `since_id`, so an
unchanged bootstrap URL cannot repeatedly replay already-received output.
An absent or empty header falls back to `since_id`. Resume removes the new
connection's capture-time cutoff: older retained segments after the cursor
must remain eligible. It starts scanning immediately, without needing another
EventBus notification.

A cursor is bound to the same pane selection. Reusing a pane-7 cursor for pane
8 or for all panes fails with HTTP 400 and `delta_cursor_scope_mismatch`.
Starting a deliberately different selection requires a fresh subscription or
an explicit numeric `since_id` bootstrap appropriate to that selection.
Malformed, overflowing, or non-UTF-8 cursors fail with HTTP 400 and
`invalid_delta_cursor`; raw cursor input is not echoed in errors. Decimal
segment ids are bounded by the signed 64-bit storage id range.

## Legacy clients and control frames

Earlier versions emitted connection-local numeric sequence ids. Those ids
cannot safely identify persisted segments and are rejected as reconnect
headers, not guessed or silently treated as fresh subscriptions. A client
migrating from that format can bootstrap with the actual last received
`data.segment_id` through `since_id`. Without that information, it must choose
an explicit history or fresh-subscription boundary.

The JSON envelope's `seq` remains per connection. Only `delta` frames set the
SSE `id` field. `ready`, `gap`, `lag`, `error`, and keepalive comments carry no
id field at all; in particular, they do not send an empty `id:` that would
clear the browser's cursor. The ready payload reports `cursor_format`,
`resume_after_id`, and `recovery: retained_segments_only`.

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

Recovery covers appended segments that remain in the same capture database.
It does not recreate retention-deleted output, replay historical capture-gap
notifications, detect database replacement or restore, or replay in-place
changes to previously delivered rows. Sparse ids alone do not prove deletion,
because filtered panes also create holes. Do not reuse these cursors across
database replacement or an unrelated server. A fresh connection that fails
before receiving its first delta has no stored SSE cursor unless the caller
provided an explicit bootstrap boundary. These limits are not a claim of
unconditional lossless terminal capture.

## Regression coverage

`web::sse::delta::tests` contains source-level regressions for bounded-page
catch-up, full-channel delivery, shutdown, query failures, malformed pages,
redaction, cursor parsing and scope, control-frame ids, historical resume, and
disconnect with unread queued frames. Tests were added with this implementation
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
resuming the page does not override an explicit pause. Closing the panel clears
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
Malformed frames do not advance the received cursor. Gap/lag notifications,
retention truncation, server failures, and reconnection before the first cursor
remain visible. The database-identity and retained-history limits above apply.

Build and deploy a fresh browser package through the approved build lane;
updating only HTML is insufficient because the viewer adds the `selectedPaneId`
WASM export. The package builder includes `pane-output.mjs` in its hashed asset
set. `node --test tests/browser/pane-output.test.mjs` exercises the production
controller and panel with controlled transport/DOM/model dependencies. These
tests do not establish a compiled WASM binding, native rendering, browser
accessibility qualification, or a real FrankenTerm HTTP/SQLite end-to-end pass.
