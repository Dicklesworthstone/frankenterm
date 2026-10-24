# Pane user variables across mux attachment

This correction follows the attachment bug reported and diagnosed by
[oysteinkrog in PR #125](https://github.com/Dicklesworthstone/frankenterm/pull/125).
It reuses the contribution's distinction between initial state and subsequent
changes, with request-driven snapshots in place of listing-generated alerts.

## Contract

After a client discovers a pane, its user-variable map must converge without
rendering, focusing, or sending input to that pane. Reconnect and topology
resynchronization must replace the complete map, including removal of keys
that disappeared from the source. A missing pane must not be recreated by a
late response.

User-variable state and historical occurrences have different consumers.
`format-tab-title` reads current state. `user-var-changed` consumes individual
live occurrences, including repeated assignments of the same value. Installing
a snapshot invalidates the owning window when the map changes; it does not
manufacture historical Lua events.

This implements the user-variable portion of the attachment and latest-state
contracts described by `ft-interactive-systems-performance-4tenz.5.5.7` and
`.5.5.8`. It does not close those broader beads or activate dormant ordered
topology capabilities.

## Discovery and compatibility

Codec 70 adds `GetPaneUserVarsV1` (PDU115) and its correlated response (PDU116).
A response contains either a complete map, including an empty map, or
`PaneGone`. Existing listings and `NotifyAlert` retain their wire schemas.
The ordinary minimum supported codec remains 61.

The client starts metadata requests after pane mappings and RPC readiness
commit. It uses the same path after legacy, coherent, or ordered pane census
application. Current `TopologyCapabilities::SERVER_SUPPORTED` deliberately
omits ordered-window support, so production ordered discovery still falls
back to the coherent census. Metadata synchronization does not change that
policy.

Older negotiated codecs do not receive PDU115. They remain compatible, but
cannot recover pre-attachment background values through this new protocol.
The server forwards subsequent `SetUserVar` notifications using the existing
alert PDU regardless of whether a session registered the pane for rendering.
Other alert kinds retain their existing rendering behavior.

## Publication ordering

A local snapshot captures its map and inserts a publication fence under the
same producer/model locks used to publish pane alerts. The server captures
the exact pane registration when it admits the request. Its snapshot guard
also retains that registration's output continuation.

The response waits for earlier ordinary alert publication, then enters the
connection FIFO while the guard is still alive. Dropping the guard permits
later alert publication and pane retirement to continue. No rendering
registration or per-session set of "already announced" panes is necessary.

Historical callback completion has a separate FIFO. Ordinary publication
finishes before historical callbacks run, so a callback can await a metadata
RPC without waiting on itself. Shared predecessor chains preserve ordering
when an intermediate producer or snapshot is canceled. Canceling one
`wait_ready` future does not erase the snapshot's predecessor.

On the client, ready-state live user-variable alerts and snapshot responses
share a bounded delivery queue. A metadata RPC completes only after its
snapshot has been applied. Delivery checks attachment generation and pane
registration so delayed work cannot mutate a replacement owner. The existing
bounded pre-ready queue remains responsible for genuine startup occurrences.

## Work and memory bounds

The metadata coordinator admits at most four concurrent pane requests,
including retries. A large population is processed to completion after
transport readiness, rather than being cut off by a listing notification
limit or charged against the attachment timeout. Repeated census requests
coalesce. New live keys trigger bounded refresh work because the terminal
may silently evict another key when adding one. Targeted refreshes coalesce
by pane ID; more than 4,096 distinct pending targets promote the obligation
to a single complete census instead of growing the target set.

A single map admits at most 16,384 entries and 7,864,256 aggregate UTF-8 bytes
across names and values. The decompressed response body is capped at 8 MiB.
The decoder checks container and string lengths before allocation, rejects
duplicate keys, and rejects trailing schema bytes. Encoding sorts keys for a
deterministic representation. Source capture copies into a freshly reserved
map so an oversized, sparsely populated source table cannot bypass the bounds.

Source work reserves 16 MiB of main-thread task admission while its snapshot
is retained. Source-lock contention returns an explicitly retryable refusal;
it never waits behind parser or durability-writer backpressure on the main
thread. Before encoding, a separate main-thread admission charges the larger
of the codec's planned buffer peak and the logical payload plus two bounded
frame allocations. This covers serialization, compression, and buffer-growth
overlap while the typed map remains funded by the source reservation. It is
estimated buffer accounting, not a process-RSS or native zstd workspace cap.
Outbound snapshot retention uses the existing separate 16 MiB snapshot byte
budget and preserves FIFO ownership through partial writes and flushes.
Temporary scheduler or snapshot-byte pressure returns a typed retryable
response; four concurrent requests do not require increasing those budgets.
The client retries within its four slots and fails the exact generation
explicitly if its finite retry budget is exhausted.
Client delivery accounts for retained frame capacity, including the frame
currently being applied, within 4,096 frames and 64 MiB. Its live map has room
for two bounded snapshot
windows while a targeted refresh is outstanding, so adding a key to a full
source map can converge after source eviction without immediate rejection.
Further growth is rejected explicitly rather than retained without a bound.
Oversized authoritative state is rejected rather than truncated.

## Regression coverage and validation status

The added codec tests cover empty/populated/missing responses, maximum map
count and text, deterministic encoding, hostile lengths, duplicate keys,
trailing bytes, and codec-69 compatibility.

Mux source tests cover pre-registration terminal state, cancellation of a
middle fence, cancellation and retry of a wait, exact bounds, sparse source
tables, nonblocking lock contention, and an actual `LocalPane` historical
callback awaiting a snapshot while its receipt remains outstanding.

Client tests cover authoritative replacement, empty maps, exact ownership,
application acknowledgment, repeated live occurrences, queue capacity and
cancellation, large populations, reconnect, compatibility, and retry policy.
Dispatcher tests cover snapshot retention, a stalled writer, maximum-size
maps, temporary byte pressure, and release of codec-stage admission.

`tests/dispatch_conformance.rs` exercises the production server dispatch,
codec, bounded RPC scheduler, mux registrations, and terminal OSC parser over
a controlled interactive byte transport. Its scenarios include two unrendered
tabs, all listing selections and ordered fallback, later assignments,
reconnect, eviction, resync, removal, a partially written snapshot followed by
a live update, and two full passes over 600 panes. This is not a native-socket,
real-PTY, or GUI test.

At implementation time on 2026-10-12, pinned Rustfmt and the codec-history
guard ran successfully. Native Rust tests and Clippy could not reach
compilation: the required RCH runner was unavailable, local dependencies were
incomplete, and the build environment exhausted its storage. The macOS GUI
attachment script reported `SKIP: native macOS only` on this Linux host. The
contributor's reported 805 passing library tests and clean Clippy results have
not been independently reproduced and do not validate this reimplementation.

The native acceptance run still needs codec, mux, client, and mux-server
library tests; `dispatch_conformance`; the mux snapshot tests with
`disruptor-pane-io`; and Clippy for those four packages. GUI acceptance should
set user variables in multiple tabs, restart only the GUI, and verify
`format-tab-title` for every background tab before focusing it. Repeat with
live changes, reconnect, resynchronization, and removal while a metadata
response is delayed.
