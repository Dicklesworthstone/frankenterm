# Bounded native SSH transport

This optional native package adapts the plan's selected libssh2 backend to an
explicit, nonblocking mobile ownership boundary. It implements real SSH key
exchange, peer-key inspection, asynchronous external public-key signing and password authentication,
PTY/shell channel creation, ordered byte I/O, window changes and teardown. It
contains no alternate ANSI parser, cryptography implementation or Rust runtime.
The existing FrankenTerm terminal, keyboard and Metal components remain canonical.

The iOS live workspace uses this backend with the canonical terminal and managed
connection owner. Apple builds supply target-specific libssh2 and crypto artifacts;
a desktop dylib cannot serve as an iPhone artifact. New features still require
Apple SDK and device qualification. Releases use DSR; Rust development proof uses RCH.

## Ownership and security contract

A successful `ftssh_create` consumes exactly one already-connected stream socket;
a failed creation leaves that descriptor with its caller. The caller serializes
all handle calls. The Swift owners below provide DNS, TCP connection deadlines,
readiness and cancellation; the application retains workspace authority. The
driver never reads a filesystem key, enumerates an agent, or installs a global
SIGPIPE handler. Passwords enter only through the explicit bounded API below.
`ftssh_close` aborts this one
owned connection without relying on a remote goodbye; it does not promise remote
process survival. The C header and implementation document the native exception.

The fixed initial profile allows SHA-2 ECDH/X25519 key exchange, modern host-key
signatures, AES-CTR with SHA-2 encrypt-then-MAC, and no compression. The caller must
check the actual negotiated methods against every imported route constraint;
unsupported mandatory policy must block a connection, not silently use defaults.
Host keys are copied only after libssh2 verifies the exchange. Authentication is
unavailable until the caller explicitly approves that exact key under its current
endpoint/alias/revocation policy. Reading a key and immediately approving it without
that policy is NOT host verification.

Public-key authentication supports Ed25519, ECDSA P-256/P-384/P-521, and RSA
with 2048–8192-bit moduli and a positive odd exponent of at most 64 bits. The callback
validates/captures one bounded RFC 4252 preimage and returns libssh2's supported
EAGAIN while the native broker obtains a signature. On reentry the preimage must
match exactly. The supplied SSH signature is unwrapped once; DER is not an SSH
ECDSA signature. No callback blocks a Swift executor or invokes Swift under a
native stack. Rejected authentication never automatically falls back or opens a
shell. Opening the shell is a separate explicit operation.

RSA keeps the canonical `ssh-rsa` public blob and fingerprint but permits only
`rsa-sha2-256` or `rsa-sha2-512` in the signed authentication preimage and returned
signature. The native limits are 1,053 public-key bytes, 1,536 challenge bytes,
and 1,044 encoded signature bytes. The raw RSA signature must have exactly the
modulus width. A different hash, certificate name, malformed integer, or reused
challenge cannot reach signing or consume the signature.

libssh2 can offer an unsigned `ssh-rsa` request when the server does not advertise
an intersecting SHA-2 signature algorithm. Its public API cannot expose that
choice before the signing callback. The bridge refuses a resulting SHA-1
challenge without calling the signer; an unsigned RSA refusal is terminal and
does not authorize automatic key iteration. There is no SHA-1 signature fallback.

Password authentication requires the same exact host approval. The driver checks
the server's bounded method list before returning `FTSSH_PASSWORD_NEEDED`, then
accepts one 1–1024-byte owned response. Its retained buffer survives EAGAIN and is
cleared on success, failure and close. Partial authentication, password changes,
keyboard-interactive substitution and success without the selected method are
refused. A password failure never authorizes another key or password attempt.

One 16 KiB adapter slot accepts a complete bounded write or nothing. Once accepted,
its storage and its exact native write arguments survive EAGAIN. A caller must NOT
interpret EAGAIN from libssh2 itself as permission to replace its pending buffer.
`accepted_bytes` is local adapter admission; `channel_accepted_bytes` advances only
after libssh2 accepts the payload and its ciphertext drains through every owned
hop into the root socket. Neither is command execution or remote PTY delivery.
A close can leave the difference uncertain and authorizes no replay. Terminal EOF
is exposed separately and does not fabricate an exit status of zero. After EOF,
`ftssh_completion_step` advances channel-close processing with a 32 KiB receive quantum
per call and copies a bounded signal name or nonzero UInt32 exit status. The
pinned libssh2 initializes its exit-status field to zero without exposing a
received flag: an actual zero and an absent status therefore both remain unknown.

Custom allocation caps bound libssh2 allocations and each handle's 64 KiB owned
ciphertext queue, including allocator overhead
and replacement overlap, to a caller-selected 64 KiB–4 MiB per handle and 16 MiB
process-wide. There are at most 32 handles. These caps do not account for the
crypto backend's own allocations or OS socket buffers; whole-app accounting still
needs device measurement. Receive windows, read calls and the one write slot are
bounded independently. No unbounded output queue or writer thread is introduced.

Up to three ordered jump hosts support independent public-key or password logins. `forward` consumes its
authenticated owner and transfers the complete ancestor chain to the returned
session. Each next host performs an independent handshake, pin check and login;
no transit PTY or shell opens. Owned ciphertext queues separate nested library
callbacks from forwarding writes and retain each exact span across EAGAIN.
Allocation statistics and close include all ancestors. A fourth jump is refused.

## Executed validation

The current C source builds with `-Wall -Wextra -Werror` against pinned libssh2
1.11.1. All 37 Python unittest scenarios passed with an independent libssh server over actual
IPv4/IPv6 loopback TCP, with ephemeral test-only keys generated by Python
cryptography. They check Ed25519, all three supported ECDSA curves, RSA, exact RFC 4252 fields,
deferred signing, rejection before signing, peer mismatch before authentication,
PTY dimensions, merged extended output, Unicode, resize, EOF, half-megabyte
backpressure/FIFO preservation, cancellation, resource caps and released ownership.
The current run additionally covers one-, two- and three-hop bidirectional
backpressure, the hop bound, deep peer mismatch before credential offer, and
release of every ancestor while ciphertext remains pending. No private user key
or external host is involved.

Seven native tests cover password method/host gates, bounded copied UTF-8
responses, terminal refusals, cancellation and mixed password/key forwarding.
Seven further native tests cover RSA at 2048, 4096 and 8192 bits with both SHA-2
hashes, wrong algorithms and signature widths, cryptographic rejection of a bad
digest, malformed keys, SHA-1 refusal, unsigned-refusal semantics, and an RSA
destination behind a separately authenticated hop.

Before the RSA integration, `Tests/Interop/test_password.py` passed all 13 live Swift scenarios, including
prompt expiry, late-result rejection, live/ancestor revocation and terminal I/O.
All 14 existing Swift actor interoperability tests passed against the same build.
That complete Swift SSH package run reported 62 passed tests and two ordinary-run live
fixture skips. The complete Kit suite reported 1,159 passed tests with no failures
or skips, including the shared password broker's queue and retirement checks.
Both packages compiled with warnings treated as errors. The iOS prompt/controller
changes have syntax checks and six added tests awaiting an Apple host.

The RSA integration's 13 changed Swift source/test files passed syntax parsing,
but disk exhaustion stopped the pinned SwiftPM build before changed-source
compilation. Its new RSA importer, signer, broker and live Swift interoperability
tests have not executed. The earlier Swift results do not validate these changes;
CryptoExtras linking and the phone's protected signing path also await Apple
SDK/device qualification.

From this directory, with the system development/test dependencies installed:

```sh
swift build --only-use-versions-from-resolved-file -j 2
cc -std=c11 -D_DEFAULT_SOURCE -Wall -Wextra -Werror -fPIC -shared \
  -I Sources/CFrankenTermSSH/include Sources/CFrankenTermSSH/FrankenTermSSH.c \
  -lssh2 -pthread -o /tmp/libfrankenterm-ssh-tests.so
python Tests/Native/test_native.py /tmp/libfrankenterm-ssh-tests.so
```

The independent server uses libssh only in tests. Apple products use the existing
selected libssh2 backend, not the test server library. These results do not certify
an Apple SDK build, native Keychain prompts, the Rust bridge, Metal, a running
remote shell process or a physical iPhone. SSH protocol interoperability and
application/device qualification are separate evidence classes.

## Swift session ownership

The `FrankenTermSSH` product adds `NativeSSHSession`, an actor that consumes a
connected descriptor and drives the actual native backend. Its mandatory
`NativeSSHAuthentication` provider owns endpoint/revision/trust/algorithm policy
and protected signing; there is no default trust acceptance. The provider gets
actual negotiated methods and the verified exchange's host key before a public
identity can be offered. A typed, exact-bound user-authentication challenge is
supplied to the signer, then policy is rechecked before signature submission,
authentication publication and explicit PTY/shell creation. The optional integration
product supplies `WorkspaceNativeSSHAuthentication` for the existing workspace
broker and protected public-key discovery. The iOS live workspace uses this
adapter for its explicitly selected direct-SSH connections.

`NativeSSHPasswordAuthentication` separates verified account selection from the
one-use prompt. `WorkspaceNativeSSHPasswordAuthentication` binds it to the actual
transport, route revision, destination graph, permitted method and host pin. The
shared broker serializes password and key prompts under its existing deadline.
Pending passwords additionally belong to a foreground epoch; completed logins
retain ordinary durable-policy checks without retaining the password.

Authentication and shell creation use finite ContinuousClock deadlines. A stale
expiry callback cannot close a later operation. Cancellation or expiry invalidates
the native handle before awaiting cleanup and signals the signing provider; an
uncooperative late provider result is discarded, not used to reopen the source.
The provider must still settle its own protected worker. The actor does not spawn
one task per byte, implement an ANSI encoder, or expose arbitrary-message signing.

`admit` reports whole-unit local acceptance or would-block. One caller drives
`service` to flush that stable native slot, deliver at most 16 KiB to the canonical
terminal, and complete a requested resize. Admission wakes an idle service call.
A service readiness timeout reports `waitExpired` while preserving the exact
connection and pending bytes; a quiet terminal is not disconnected automatically.
Cancellation, native failure and explicit close remain separate from idle expiry.
After EOF, new writes are refused and the actor gives channel-close/status
evidence a two-second absolute deadline. Readiness waits yield normally; receive
budget exhaustion yields cooperatively rather than extending that deadline.
`NativeSSHCompletion` reports observed closure separately from an optional
nonzero status or signal. EOF-only expiry stays unconfirmed, and neither zero
nor absent status becomes a success claim. Cancellation and transport loss
remain distinct from this completion result.

Socket waits use DispatchSource readiness and one timer, not periodic polling.
Each registration owns a duplicated fd until every cancellation handler settles,
preventing stale readiness callbacks from targeting a reused descriptor. One
shared readiness queue serves all sessions. Concurrent close calls await the same
cleanup. The application still owns foreground/network permission, route policy,
reconnect decisions, durable uncertainty markers, and terminal binding.

`keepalive` sends one liveness probe, a keepalive request that asks the server to
reply; `receivedBytes` counts every byte read from the server, replies included.
libssh2 binds a partly sent packet to its caller's buffer and refuses all other
packets until that buffer is resent, and its keepalive buffer is a stack frame.
So the probe is sent only when no write, resize or library packet is pending and
the socket buffer has room for the whole packet (`SO_NWRITE`, or `SIOCOUTQ` on
Linux); otherwise it is deferred. libssh2 also reports success for a keepalive
it skips because the last one left under 2 seconds before, so the session counts
the bytes handed to the socket and reports a probe that sent nothing as deferred;
no reply is then due. If a packet is still pending afterward, the
session fails rather than block later input. `WorkspaceSSHLiveness` is the
connection policy: after each quiet interval (10 s standard) it probes, and after
two further silent intervals it closes the connection and the session ends as
disconnected. A socket failure that precedes any owner's close also ends as
disconnected, not as a local close.

## Imported workspace authentication and connection ownership

`WorkspaceNativeSSHAuthentication` implements the native authentication provider
with `FrankenTermKit.SSHAuthenticationBroker`. Configure the shared foreground
broker with the adapter's `routeSupport`: direct SSH or FrankenTerm mux routes,
up to three ordered jumps, pinned Ed25519/NIST ECDSA/RSA-SHA2 host keys, and
Ed25519/P-256/P-384/P-521/RSA-SHA2 client keys or an explicit SSH password method. Excessive
jumps, certificates, interactive MFA, password changes and
unsupported mandatory source policies remain blocked.
The adapter compares the actual negotiated methods, original endpoint, selected
route revision, host pin and transport identity before looking up a public key.
The broker checks current protected storage and local revocations before and after
lookup. New Keychain items provide validated public metadata without an unlock;
older items can derive their public half on the same protected worker. An explicit
reviewed reimport can enrich missing public metadata on that same account without
replacing its private value or access controls. The existing fingerprint/comment
format is validated canonically; malformed metadata never triggers a fallback to
private-key access. Every incoming private/public pair is derived and checked
before any reusable item's metadata is changed.

The signer reconstructs the exact typed RFC 4252 challenge rather than signing
arbitrary driver bytes. A selected identity carries an unforgeable foreground
lifetime witness. Suspension/resume, reimport, revocation or cancellation cannot
revive it, including cancellation before work reaches the broker. The single
protected-worker reservation remains held until cancelled native work settles.
`validateCurrent()` repeats durable policy checks for a retained live connection.

The phone shows one SecureField prompt for the visible destination, with its
verified account/endpoint and host fingerprint. Leaving the app or destination
clears and cancels it. Password routes do not warm in hidden tabs or reconnect
automatically. After a password hop, retrying a refused destination key or falling
back from mux to direct SSH requires an explicit Retry. Only the remaining typed
choice survives; the password never enters saved workspace or session history.

`WorkspaceNativeSSHConnection.connect` adds explicit user confirmation, current
machine/route/credential readiness, and foreground checks before DNS. Its shared
`WorkspaceNetworkActivity` lease excludes strict Offline Restore until owned
network work has joined, including uncancellable resolver work. Requesting
restoration while connected reports the conflict; it never silently closes a
remote shell. The owner's raw session does not escape. Its separate `openTerminal`
operation has a production journal overload that durably records the exact source
before creating a PTY, claims the package driver, and attaches the existing coordinator/pump. Writes,
service and resize revalidate foreground and current authority. Close joins native
teardown before releasing network exclusion. The caller still owns canonical
terminal consumption, scene lifecycle, and retained input-outcome reconciliation.

## Durable creation and managed connections

`TerminalSessionJournal` stores session-creation uncertainty in a **dedicated**
protected persistence directory, separate from the workspace catalog and import
journal. `beginCreation(scope:)` returns an opaque epoch only after its atomic
write and durability confirmation succeed. The record binds the installed
workspace, saved revision, machine, route, transport and channel. It contains no
terminal input/output, private keys, hostnames or content-derived hashes.

An unresolved record blocks another creation for the same installed workspace and
machine, even after a route or revision changes. Passing the journal to the native
connection factory checks that record before DNS. Before the pump is published,
the journal binds its exact input source to the creation epoch. `recordInput`
persists only a bounded union of transport observations, never commands, paste
contents or replay instructions. A coordinator-issued `TerminalInputSeal` proves
local input settlement before `sealInput` records interruption; an opening that
never bound input uses `markInterrupted` after native close. Neither transport
acceptance nor SSH EOF proves remote execution or process exit. After relaunch,
records become available for deliberate recovery review. `resolve(_:)` requires
the exact displayed journal-issued record and refuses active or changed history.
The default bounds are 64 unresolved and 32 acknowledged records; only
acknowledged history is pruned. File-backed journals anchor their identity outside
the catalog (device-only Keychain on Apple), so missing history cannot silently
authorize a replacement shell.

`WorkspaceSSHConnectionManager` is the app-owned entry point shared by scenes.
Construct it with the workspace store, foreground broker, dedicated journal,
input coordinator and shared `WorkspaceNetworkActivity`. It starts with networking
disabled. `resume()` enables foreground admission, and each `connect` separately
requires `confirmedByUser: true`. Admission is bounded to three attempts and at
most sixteen retained connection/settlement owners, with one owner per installed
workspace and machine. There is no waiting queue or automatic reconnect.

The returned `WorkspaceSSHConnection` is a facade over the existing native owner;
it exposes neither the session nor an alternative authentication/transport path.
Its separate `openTerminal(geometry:)` always uses the retained journal and
coordinator and returns the canonical `TerminalSessionPump`. The scene retains
the task running that pump with its terminal consumer. Source-bound
`requestResize(_:source:)` keeps the first pending geometry immutable; another
request returns busy without disconnecting, and can be retried after its service
completion.

On backgrounding, `suspend()` seals every live input source before joining any
blocked prompt, journal write or socket teardown. Foreground transitions coalesce
through one retained driver; an older suspension cannot override a newer resume.
Repeated suspend, invalidation and close requests share one settlement job per
exact owner. `invalidate(workspaceID:)` closes affected owners after reimport,
revocation or policy changes, while native I/O also checks current durable policy.

Use `manager.close(connectionID:)` when the pump ends, including EOF/error. The
native owner closes I/O and joins an in-progress opening task. Concurrent closes
share native shutdown and the subsequent unpublished-source retirement/journal
settlement attempt. For a published source, the scene must join its consumer and
all keyboard/focus streams, persist retained outcomes through
`connection.recordInput(_:source:)`, and call
`connection.prepareInputSettlement(source:)` **before** retiring the coordinator
source. That method caches the source-wide seal; it refuses while admission,
streams, writes or outcomes remain active. An early close can therefore stop I/O
while retaining the manager's settlement owner for the scene's final close.

Once input settles, the final close publishes its journal evidence. Storage
failure preserves the exact epoch and cached seal for a later close retry, even
after the source has been retired. Retrying never creates a shell or replays
input. The live iOS controller performs this sequence, and its presenter persists
each outcome batch before acknowledging delivery. The native owner's compatibility
durability-callback overload remains available for custom integrations, which
retain responsibility for their own markers and recovery.

## Binding a native channel to terminal input and output

The optional `FrankenTermSSHIntegration` product joins the native channel to
`FrankenTermKit`'s actual `TerminalSessionPump`. The offline Kit package has no SSH
dependency. Apple builds use the Kit's system dependencies; Linux package builds
also resolve its existing pinned Swift Crypto dependency. After authorization,
authentication, explicit shell creation and preparation of the app's durable
uncertainty marker,
construct a `TerminalConnectionIdentity` with the session's `transportID` and
`channelID`, then call
`NativeSSHTerminalAttachment.attach(session:identity:coordinator:)`. The attachment
publishes one immutable `source` and its `pump`. The app supplies the canonical
terminal consumer and retains the task running `pump.run(consumer:)`.

The session grants one package-scoped driver capability only before any raw
channel input, output or resize has been driven. After that transfer, raw
`admit`, `service` and `requestResize` calls are refused even if the caller supplies
the attachment's public channel UUID. Legacy `wakeService(channelID:)` is a no-op
for a capability-owned channel; only its private driver can wake it. Public
`close` remains available for policy revocation. The transport itself stays
private to its attachment and cannot be reused with a second coordinator.
Failure after a native claim closes that session rather than returning a partially
owned channel to the caller.

Coordinator leases of up to 64 KiB are accepted in at most 16 KiB native prefixes,
preserving the remaining bytes in the existing coordinator. There is no extra
adapter queue. A coalesced wake interrupts an idle native service even if no peer
output arrives, and a wake sent before service is retained. Every woken turn still
services bounded network work so an input flood cannot starve output.

`requestResize(_:source:)` requires the exact attachment source and permits only
one immutable native request at a time. Completion is returned through the same
pump and means local window-change transmission, not an SSH server geometry ACK.
`close()` closes coordinator admission and joins native teardown, without waiting
on a currently suspended consumer callback. The app separately joins its pump
task and reconciles retained outcomes. `accounting()` remains available afterward
with separate local and channel acceptance counts; neither proves remote effects
or permits replay on a replacement connection.

The published `NativeSSHTerminalChannel.attach(session:identity:coordinator:)`
entry point remains available for compatibility and returns its source-owned
pump. It uses the older explicit `claimTerminalChannel(_:)` UUID-bound surface,
including `requestResize(_:channelID:)`. A matching UUID authorizes raw I/O on
that legacy channel; it is not the private capability used by the recommended
`NativeSSHTerminalAttachment`. Legacy and capability claims exclude one another,
and a previously consumed channel cannot be relabeled under either API. A wrong
transport or competing claim leaves the existing owner's connection intact.

The pump delivers each bounded byte chunk to the canonical terminal consumer and
routes answerbacks through the same coordinator. It does not parse ANSI, encode
keys, manufacture frames, or echo input locally. Disposition batches are peeked,
delivered and then acknowledged; a failed consumer retains its batch and bounded
accounting charge. Cancellation closes coordinator admission even while delivery
is suspended, aborts native I/O and joins the wake child. Completion of `run`
additionally joins its already-owned consumer work. A callback can request stop
but must not await that same run task. The native attachment maps completion to
`TerminalSessionEnd.completed`; the final consumer update carries the same
evidence after all output chunks. Transports without completion metadata retain
the older `.peerOutputEnded` result. Neither path creates another shell.

## Hostname and dual-stack connection establishment

`NativeSSHConnector.connect(to:)` accepts a validated canonical ASCII hostname or
unbracketed IPv4/IPv6 literal and a port, resolves it locally, and returns an
unauthenticated `NativeSSHSession`. The original immutable endpoint accompanies
the actual host key and negotiated methods; falling back to a different IP must
not replace the host/alias identity used by the application's trust policy.
Creating TCP does not offer a credential, start SSH or create a remote shell.
The caller must explicitly authorize network use and respect Connect Later and
strict offline restoration before invoking the connector.

The resolver has four process-wide worker slots and no unbounded waiting queue.
POSIX `getaddrinfo` cannot be cancelled. Cancellation/deadline resumes its caller
promptly but retains the worker reservation until native work finishes; late
results are freed and never used to connect. At most eight copied addresses are
retained from at most 64 inspected results. These are application-state bounds,
not a claim to bound allocations inside the OS resolver.

IPv6/IPv4 candidates are interleaved while preserving resolver order within each
family. At most two connection attempts race, with a 250 ms initial stagger,
five-second per-address limit and one shared total deadline. Connecting sockets
have a separate 32-slot process-wide cap. Every candidate owns its nonblocking,
close-on-exec descriptor; success checks SO_ERROR rather than assuming writability
means connection. Losers are cancelled and joined, and ownership transfers only
after native session creation succeeds. Failure and cancellation leave no orphan
socket and cannot start authentication through a stale result.

The connector is deliberately not an SSH-config evaluator or a replacement for
the workspace's IDNA/trust canonicalization. It does not implement proxy-command
execution, jump hosts, automatic reconnect, permission UI or session routing.
Those remain separate features with explicit authority and continuity contracts.

## Combined Swift validation

The current package run executes **41 tests: 40 pass and one live-server entry
point skips**, with warnings as errors. Independent libssh harnesses separately
pass **12 native-driver methods, 13 Swift interop methods, 12 workspace methods
and 8 legacy-pump methods**. The completion scenarios cover delayed status after
EOF, full UInt32 nonzero status, signal, reported zero, absent status, bounded
EOF-only expiry, transport reset, cancellation, receive-budget exhaustion and
post-EOF admission refusal. The C adapter compiles with
`-std=c11 -Wall -Wextra -Werror`; stats ABI guard bytes include `received_bytes`.

Ten Swift XCTest cases pass with warnings-as-errors, including 64 repeated
concurrent-close cycles with fd accounting, stalled handshakes, cancellation,
deadlines, phase/admission bounds, retained closed-session handle release and
strict typed-challenge truncation checks. Nine connector tests additionally cover
numeric IPv4, IPv6, hostname fallback, refused sockets, input/deadline validation,
pre-cancelled and concurrent cancelled work, budget recovery and losing-descriptor
cleanup.
Four attachment tests additionally reject unauthenticated sockets, mismatched
native transport/channel identities, pre-cancelled attachment and closed-channel
revival without publishing a source or consuming caller-owned live state. All
twenty-three cases remain in the complete package run. With the workspace
authentication, route-admission and managed-connection cases, **40 XCTest cases pass** under
Swift 6.2.1 with warnings-as-errors. One additional live-server entry point is
explicitly skipped in an ordinary run, then executed by the harness below.
Nine additional integration tests drive the production Swift actor and C adapter
against the independent server: four signing families, exact Unicode/binary data,
resize, wake-up, idle timeout followed by working input, rejected peer policy,
late cancelled/expired signatures, and revocation before sign or shell creation.
The hostname-path case resolves localhost, establishes TCP itself, then performs
real authentication, PTY/shell protocol requests, resize and exact byte transfer.
The terminal-pump path also runs all four signing families and verifies a 202 KB
payload through 64 KiB coordinator leases and 16 KiB native prefixes, two-operation
FIFO order, exact final bytes, source-bound resize completion, EOF, retained
acceptance accounting, duplicate attachment and raw-I/O refusal, latched wake
before/during idle service, and prompt cancellation of a quiet reader. These are
separate from the nine native-driver scenarios above.

Eight additional real-server scenarios exercise the production workspace import,
canonical public-key discovery, actual signing broker, native connector/libssh2,
and independent libssh server together. They verify exact Unicode PTY bytes,
rejection of wrong/revoked host keys before credential access, credential
revocation before shell creation, and the complete route owner/coordinator/pump
flow with resize. The owned flow also proves network/offline exclusion and joined
release, failure of the app's marker callback before any shell request, refusal
of queued input after revocation, and closed coordinator admission before the
pump has started. The harness runs the live XCTest entry point once per scenario
and rejects a skipped or undiscovered test. Its synthetic memory-backed vault uses
the real production signer; native Keychain prompts and device durability remain
separate qualification work.

The optional `WorkspaceSSHProbe` adds twenty real exchanges to those eight
scenarios. These cover all four client-key families, independently generated
OpenSSH full-block padding, canonical hostname handling, delayed credential/host
revocation and reimport, cancellation while signing, and rejection before shell
creation. Managed flows use the real protected-file journal: the independent
server reads the durable marker before accepting any channel, PTY or shell
request. A parked journal write proves suspension joins late publication without
creating a shell or leaking a source, and a newer resume remains effective. The
managed echo flow also checks exact terminal bytes, retained outcomes, duplicate
creation refusal, explicit review, network exclusion and nonfatal resize busy.
Its input evidence is durable before outcome acknowledgement, and an active
responder prevents final settlement even after native I/O closes. A final-write
failure then retries successfully after coordinator source retirement without
replaying input or retaining native resources. Another flow parks journal binding
after actual shell creation and overlaps opening-failure cleanup, manager close
and eight direct closes; every close succeeds, one unpublished source retires,
and the server observes exactly one shell with no terminal input. Together the
workspace harness passed 28 exchanges across 12 test methods. The ordinary SSH
package suite passed 40 cases, with its live-server entry point skipped there
and exercised by the harness; both Swift builds treated warnings as errors.

```sh
swift test --only-use-versions-from-resolved-file -j 2 -Xswiftc -warnings-as-errors
swift build --only-use-versions-from-resolved-file --product FrankenTermSSHInteropProbe -j 2 -Xswiftc -warnings-as-errors
python Tests/Interop/test_swift.py /tmp/libfrankenterm-ssh-tests.so .build/debug/FrankenTermSSHInteropProbe
swift build --only-use-versions-from-resolved-file --product TerminalPumpProbe -j 2 -Xswiftc -warnings-as-errors
python Tests/Interop/test_pump.py /tmp/libfrankenterm-ssh-tests.so .build/debug/TerminalPumpProbe
python Tests/Interop/test_workspace.py /tmp/libfrankenterm-ssh-tests.so .build/debug/FrankenTermSSHPackageTests.xctest
swift build --only-use-versions-from-resolved-file --product WorkspaceSSHProbe -j 2 -Xswiftc -warnings-as-errors
python Tests/Interop/test_workspace.py /tmp/libfrankenterm-ssh-tests.so .build/debug/FrankenTermSSHPackageTests.xctest .build/debug/WorkspaceSSHProbe
```

On macOS, `swift build --build-tests` makes one bundle per test target, so the
harness takes `.build/debug/FrankenTermSSHTests.xctest` (run through
`xcrun xctest`) and a native library built as a `.dylib`. All 28 tests passed
that way on 2026-10-27, including the two- and three-hop chains.

The retained legacy `TerminalPumpProbe` independently covers all four signing
families, a 96,030-byte hidden-pane stream, a 70,024-byte bracketed Unicode paste,
source-owned replies, idle cancellation, failed outcome delivery and rejection
of competing or previously consumed channels. Both probes reuse the independent
libssh server and check exact host-side bytes. The server accepts PTY/shell
protocol requests and supplies synthetic echo data; it does not run a shell or
editor. Neither test-only executable enters an application's linked products.

These native/test builds do not alter the main FrankenTermKit package or add
runtime downloads. No Apple SDK typecheck, Keychain prompt or physical-device
qualification is inferred from Linux execution. Supply platform-correct native
artifacts and perform the remaining app scene/native-terminal integration before
enabling Connect in the app.
