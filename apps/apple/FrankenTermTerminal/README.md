# Canonical native terminal binding

`FrankenTermTerminal` is an optional Swift 6 product that binds the existing
`frankenterm-term` Rust engine to `FrankenTermKit`. It is an actual C ABI client,
not an alternate terminal parser. The offline workspace application does not
link this product or start network work merely by restoring a workspace.

## Implemented path

`NativeTerminalModel` implements `TerminalSessionModel` using the versioned
functions in `frankenterm/term/include/frankenterm_terminal.h`. It creates the
canonical parser and terminal, feeds complete bounded output chunks, resizes the
model, invokes its negotiated keyboard and paste encoders, transfers complete
answerbacks, and copies owned FTSF frames. No Rust pointer or borrowed frame
escapes into Swift.

`TerminalModelRuntime` in Kit joins that model to the existing
`TerminalSessionPump`, input coordinator and keyboard session. Its immutable
source comes from the actual authenticated attachment. A hidden terminal keeps
parsing and answering requests without producing new render snapshots. Final
output is consumed before EOF retires input; EOF is not a process exit code.

The keyboard uses `TerminalKeyboardTransactions` to keep encoding and local
coordinator admission in one model ordering scope. Incoming output cannot change
the sampled keyboard or bracketed-paste modes during that interval. The keyboard
still validates its original focus and owns composition, held keys and cleanup.
The transaction itself never writes to SSH or creates another input queue.

Answerback copying retains the original Rust reservation. Only whole coordinator
admission or an explicit whole drop settles its exact sequence. Cancellation
after admission settles before returning, and a coordinator rejection settles a
whole drop before failing the consumer. Input, answerbacks, paste and cleanup use
the same source-bound egress owner; none is routed through the currently visible
pane.

One operation owns model mutation/publication at a time. At most one pump call
and one keyboard transaction can wait, in arrival order. The pump retains only
its current bounded chunk; an extra driver is refused. Read-only encoding may
inspect already-applied modes during a delivery callback. `requestClose()` stops
admission immediately; `close()` joins active work before releasing the native
model. A delivery callback uses `requestClose()` rather than awaiting itself.

## Connecting the components

After the application has authorized network use, verified host policy,
authenticated and explicitly created an SSH shell, use
`NativeSSHTerminalAttachment.attach(session:identity:coordinator:)` from the
optional `FrankenTermSSHIntegration` product. Construct the identity from that
session's immutable `transportID` and `channelID`.

Create `NativeTerminalModel(source:geometry:scrollbackRows:fontRevision:)` with
the attachment's exact source and the geometry used to open the PTY. Give that
model to one `TerminalModelRuntime(model:coordinator:delivery:)`, then run
`attachment.pump.run(consumer: runtime)`. The application's retained run task
owns the session lifetime. The same runtime is the encoder supplied to
`TerminalKeyboardSession`; the same source identifies its `TerminalFocus`.

The application implements `TerminalModelDelivery`:

- For `.frame(bytes)`, send the owned FTSF bytes to
  `NativeTerminalRenderView.submit(_:source:)` on MainActor. The existing renderer
  checks identity and revisions and owns its GPU resources through completion.
- For `.unavailable(reason)`, invalidate the displayed caret/input projection
  and show the unavailable viewport. An evicted scrollback viewport does not
  terminate the model; returning to the live viewport can restore presentation.
- Accept disposition batches idempotently into the session's outcome UI/state.
  Throwing leaves the complete batch in the coordinator for reconciliation.
  Successful delivery allows the pump to acknowledge exactly that batch.

Request PTY resize through the attachment, using its exact source. The pump
reports local completion to the runtime, which applies the corresponding model
geometry before the accompanying output. This is not a server acknowledgement
of geometry. Native resident-cell limits may reject a geometry accepted by the
generic transport type; preflight the chosen scrollback budget before requesting
it. The application still owns any stronger ordering barrier with previously
queued user input.

For shutdown, retire keyboard admission/cleanup, request pump stop or close the
attachment, join the retained pump task, reconcile its outcomes, and close the
model runtime. `NativeTerminalModel.close()` is idempotent; release also frees an
abandoned native frame/reply lease without replaying bytes. Close does not imply
that the remote process terminated. Locally accepted bytes remain distinct from
SSH channel acceptance and remote effects.

## Resource and feature profile

The native profile limits feeds and retained control strings to 16 KiB, committed
text to 16 KiB, encoded input/paste to 64 KiB, reply storage to 16 KiB and 128
whole units, and an owned frame to 8 MiB. A cell retains at most 256 UTF-8 bytes;
exceeding that bound closes the model instead of presenting truncated text as
faithful output. Hyperlink metadata is bounded separately.

Compact commands that expand into repeated or whole-screen work share a 65,536
step/cell budget per feed. Counts that saturate screen geometry stop at their
canonical result; genuinely expansive commands are refused before execution
when their remaining budget is insufficient. Ordinary literal/control output
retains the canonical processing path. Exhausting that command budget or losing
a newly generated reply to staging overflow closes the model explicitly, so a
partial terminal state is never published as a successful update.

There are at most 16 native sessions and one acquired frame per session. Each
session reserves at most 65,536 cells across its two screens and scrollback:
`(2 * rows + scrollbackRows) * columns`. The registry admits at most 262,144 such
cells in aggregate. Rows and columns are each at most 512 and history at most
4,096 rows. The Swift default history is 256 rows. These are explicit model
limits, not measured device-memory or performance guarantees.

This initial text profile uses the resident Legacy grid and disables image
execution, clipboard effects and unimplemented capability claims. Desktop
defaults retain their existing full profile. Unsupported render attributes and
an unavailable historical viewport are explicit presentation failures; generic
invalid model state remains a session failure. Font revisions and source IDs
must agree with the native renderer. Frames are full replacements, not partial
screen deltas.

Keyboard negotiation uses the canonical encoder, including application cursor,
newline, modify-other-keys and opt-in Kitty protocol state. Key releases follow
the negotiated mode; repeated keys use the canonical down-event semantics.
Committed text and complete accessory-key presses use paired press/release
encoding where the negotiated protocol requires it.

## Artifact and verification status

The module map imports the canonical checked-in header directly and links
`frankenterm_term`. A real C-linkable Rust artifact built from the existing term
crate with `native-ffi` is required; the default Cargo `rlib` is insufficient.
Its build must retain `panic=unwind` for the audited recoverable-panic boundary.
The consumer must supply the appropriate library search path/artifact for its
target. There is no placeholder C implementation or silent parser fallback.

The native feature rejects abort-only builds. Its recoverable boundary suppresses
the contents of caught panic payloads and delegates unmarked panics to the host's
previous hook. A host that later replaces that hook must preserve the chain and
the recoverable marker check. A caught panic poisons the affected model while
retaining conservative resource charges until its owner is released.

The Swift target has compiled with Swift 6.2.1 and warnings treated as errors
against the real header. That was a compile-only check without Rust linking.
Clang accepted the header and layout assertions as C11 and C++17. Portable Kit
tests exercise actual runtime/coordinator/keyboard ordering with an explicitly
opaque injected model; those tests do not establish Rust parser behavior.

Rust model, complete-reply and registry regression tests are included. The required
RCH tool was unavailable, so these tests have **not been compiled or executed**.
Run the native-feature and full terminal regression suites in the authorized
remote environment. Native dependency portability, static-library/XCFramework
construction, Apple SDK linking, Metal execution and device behavior remain to
be established. The existing iOS app remains an offline workspace application
until the protected authentication adapter, native artifact and scene ownership
are integrated and qualified.
