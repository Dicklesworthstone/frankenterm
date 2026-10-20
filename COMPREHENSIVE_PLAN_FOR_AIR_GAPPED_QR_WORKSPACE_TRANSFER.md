# COMPREHENSIVE PLAN FOR AIR-GAPPED QR WORKSPACE TRANSFER

> **Design extension, 2026-10-20.** Export a Mac FrankenTerm workspace, including its resolved settings and deliberately selected exportable SSH private keys, as an encrypted animated QR stream. Restore it through the native iOS camera without a network connection between the devices or a network service anywhere in the import path.
>
> **Parent plan:** [Native iOS FrankenTerm application](COMPREHENSIVE_PLAN_FOR_NATIVE_IOS_FRANKENTERM_APP.md), reviewed at `80a8b090d577e8ea33807011d0dcbea53cd1babe`. This is a new implementation track, not a claim that the native app, optical codec, or key migration already ships.
>
> **Precedence:** This document supplies the separate offline encryption/container design that parent sections 9.1 and 10.4 previously deferred. It supersedes their restriction that private-key transfer occurs only through live TLS pairing. The ordinary version-one `.ftworkspace` JSON remains private-key-free. A separately versioned encrypted **FTAG envelope** carries that manifest together with private-key records; either optical transport or a deliberately saved encrypted file can carry the same envelope. Existing input, trust, credential, revocation, and transactional-import rules remain in force.

## Contents

- [0. Product experience and nonnegotiable properties](#0-product-experience-and-nonnegotiable-properties)
- [1. What all settings and keys means](#1-what-all-settings-and-keys-means)
- [2. Threat model and two offline security modes](#2-threat-model-and-two-offline-security-modes)
- [3. Payload and encrypted envelope](#3-payload-and-encrypted-envelope)
- [4. Code-protected one-way encryption](#4-code-protected-one-way-encryption)
- [5. Recipient-bound optical encryption](#5-recipient-bound-optical-encryption)
- [6. QR framing and fountain recovery](#6-qr-framing-and-fountain-recovery)
- [7. Mac display and exportable QR video](#7-mac-display-and-exportable-qr-video)
- [8. Native iOS camera and decoder ownership](#8-native-ios-camera-and-decoder-ownership)
- [9. Safe restoration and offline operation](#9-safe-restoration-and-offline-operation)
- [10. Bounds, throughput, and failure behavior](#10-bounds-throughput-and-failure-behavior)
- [11. Architecture and implementation sequence](#11-architecture-and-implementation-sequence)
- [12. Acceptance and adversarial qualification](#12-acceptance-and-adversarial-qualification)
- [13. Parent-plan integration and primary references](#13-parent-plan-integration-and-primary-references)

---

## 0. Product experience and nonnegotiable properties

### 0.1 The default journey

On the Mac, choose **Export to iPhone → Offline QR Transfer**. Select the existing machine group and choose **Include selected SSH keys**. A preview shows machine ordering, palettes, routes, available private identities, and every setting or identity that cannot be activated on iOS.

Unlock the chosen file-backed keys locally on the Mac. The app builds one immutable export, removes duplicate private-key material, compresses the package where beneficial, encrypts the whole package, and starts a large, clean QR animation. It never displays a plaintext private key as a QR symbol.

On the phone, choose **Restore from Animated QR**. Use the in-app camera, not a browser or the system Camera app's URL-opening behavior. Point it at the Mac. Scanning may start in the middle, survive missed frames, pause when the phone moves away, and continue without discarding verified progress. A progress view reports recovered data rather than merely counting camera callbacks.

In the default one-way mode, enter a generated transfer code shown separately by the Mac. The phone verifies the complete encrypted package, previews the restoration, and publishes the workspace plus Keychain mappings through the existing recoverable import transaction. The result shows familiar machines and colors, with **Restored offline — Connect later**. Preserving an auto-connect preference does not execute that preference during offline restoration.

A second mode encrypts specifically to this phone after the Mac scans a small recipient request displayed by the phone. This also remains completely optical and offline. A Mac camera is optional, never a requirement for the default journey.

### 0.2 Required properties

1. **The video contains the data.** No URL fetch, temporary web bucket, localhost download, Bonjour bootstrap, Bluetooth payload, or hidden relay substitutes for optical transfer.
2. **The entire useful workspace travels together.** Settings and private identities bind to one immutable manifest, with an explicit completeness report.
3. **Missed frames do not imply failure.** QR error correction handles some damage inside a symbol; fountain coding handles missing whole symbols. These are different layers.
4. **No partial secrets are activated.** Recovered fragments and even complete pages remain ciphertext. Import begins only after whole-envelope authentication and full semantic validation.
5. **No implicit remote side effects.** Scanning, decrypting, and restoring do not open SSH sessions, enroll keys, run commands, or grant access to the Mac gateway.
6. **Every stage is bounded.** A hostile QR cannot request an unlimited allocation, a giant KDF, an unbounded fountain solver, or a decompression bomb.
7. **A saved video is a sensitive backup.** Encryption protects its contents under its stated mode; stopping playback does not revoke recorded ciphertext or remotely revoke a copied SSH identity.
8. **The interface is honest.** Transmission, reconstruction, decryption, validation, publication, and eventual connection have separate completion states.

## 1. What all settings and keys means

### 1.1 Preserve the selected workspace, not an arbitrary filesystem

Include group and machine IDs, order, names, tags, preferred routes, jump chains, users and endpoints, method/algorithm restrictions, host-key aliases, pins, supported CA and revocation policy, connection preferences, all resolved terminal colors, indexed-color overrides, font descriptors, keyboard mappings, and portable viewing preferences. Include the parent plan's stable-ID and merge semantics.

The export snapshot must be consistent with one local configuration revision. Required source files and key material must already be locally available. Offline export does not probe remote hosts, fetch a cloud-backed key, update a revocation source, or evaluate untrusted executable configuration to make a portability report appear complete.

Preserve a desktop-only setting as an explicitly inert annotation when useful, with its original value and a clear reason it cannot be activated. Never silently drop it and report a perfect restore. Inert annotations are data inside the encrypted container, not executable Lua, scripts, extension code, or filenames to install into a configuration search path on the phone.

The preview separates **portable and ready**, **preserved but inactive**, **missing an identity**, and **blocked by required policy**. Required omissions prevent the user from unknowingly confirming an equivalent restore; the user can deliberately accept a narrower usable workspace.

### 1.2 Private identities are explicit and deduplicated

Export only private SSH identities referenced by the selected routes and approved in the export sheet. Do not walk `~/.ssh` and export every file, dump the Mac Keychain, or offer every identity advertised by an agent. Two routes using the same canonical public key share one private record, while retaining distinct route restrictions.

For the initial private-record profile, decrypt supported source containers locally on the Mac and serialize one complete **unencrypted OpenSSH private-key container** per key inside the encrypted archive. Its container declares `ciphername=none` and `kdfname=none`; plaintext private bytes never leave the protected in-memory construction path before outer encryption. This avoids making the phone run a separate user-supplied password KDF for every key. The source key file is unchanged.

This is a proposed export/import capability that must be implemented for each supported key family. Failure to parse, unlock, or serialize an identity is a visible per-key blocker, not permission to export malformed material or silently replace the identity. The phone derives the canonical public key from the actual private components and checks it against the manifest and any associated user certificate before staging a Keychain item.

Mac-bound hardware keys, Secure Enclave keys, and agent identities whose private material is unavailable cannot be made exportable by encoding them differently. Preserve their references and explain the existing Via Mac or independent phone-enrollment alternative. Do not copy gateway CA/server private keys, another device's client identity, live mux capabilities, controller leases, or authentication-session handles as substitutes. Apple's [Secure Enclave documentation](https://developer.apple.com/documentation/security/protecting-keys-with-the-secure-enclave) establishes that preexisting plaintext keys cannot simply be imported into that device-generated-key path.

A certificate or revocation snapshot is preserved with its provenance and freshness requirement. Air-gapped import cannot prove current remote revocation status; a route that requires a fresh check remains unready until its policy can actually be satisfied.

### 1.3 No partial-prefix migration

The source manifest may state that private material is not included **in the ordinary JSON document**. The enclosing archive's private records resolve its credential references without changing that public format. The import preview reports which references are supplied by the envelope, already present locally, or still unavailable.

Settings, public identity, private identity, and certificate subject must agree. A supplied fingerprint is not proof of the private material's identity. Existing local revocation decisions and stronger access controls survive reimport, including import of an older video.

## 2. Threat model and two offline security modes

An optical channel removes the need for a network transport; it is not a confidential channel. A camera across the room or a screen recording can collect the animation. A malicious display can send a forged package. A compromised source or destination remains outside the protection encryption can supply.

| Mode | Required interaction | Protects against | Important limit |
|---|---|---|---|
| **Code-protected one-way transfer** | Mac displays encrypted animation; user separately enters a generated code on the phone. | Recording the animation without the code. | Someone who obtains both the code and the animation can decrypt it. |
| **Recipient-bound transfer** | Phone shows a public recipient request; Mac scans it, confirms the recipient, then displays the encrypted animation. | Passive recording of both public optical exchanges without the phone's private recipient key. | Public-key encryption alone does not authenticate who constructed the archive. |

Code-protected mode is mandatory for launch: it works with a Mac that has no camera and supports recoverable offline backup videos. Recipient-bound mode is preferred when the two optical directions are convenient, but is not a backup that survives loss of the recipient private key.

In both modes, show an **80-bit visual comparison value**: the first 20 hexadecimal characters of SHA-256 of the complete envelope, grouped as five groups of four. On a first-time source, compare the phone's value against the value on the deliberately selected Mac before trusting imported host policy. This is a user-authenticated visual channel, not an assertion embedded in the same untrusted QR. The full 256-bit digest remains the transport identity; the shorter display value is not used as a cryptographic key or an automatic authorization token.

Never label a self-asserted exporter ID, matching checksum, successful decryption, or embedded public key as a previously trusted Mac. An established authenticated exporter namespace may be reused only through the parent plan's existing trust association; otherwise require the explicit source/trust review.

## 3. Payload and encrypted envelope

### 3.1 Three independent formats

Keep these versions independent:

- The parent `.ftworkspace` JSON schema represents portable configuration.
- **FTAG v1** represents the encrypted archive, including private records.
- **FTQR1** represents screen/camera framing of the already encrypted FTAG bytes.

The same immutable FTAG bytes may be displayed live, rendered into a video, or saved as a `.ftairgap` file. A `.mp4` is only a presentation of those bytes; it is not itself the cryptographic container. Changing playback speed does not create a new export or new ciphertext.

### 3.2 Archive shape

Use a restricted deterministic CBOR encoding following [RFC 8949](https://www.rfc-editor.org/rfc/rfc8949.html). Use definite-length arrays and byte/text strings, shortest integer/length forms, valid UTF-8, and no tags, floating-point values, or indefinite-length objects. Reject noncanonical representations and trailing objects rather than allowing native decoders to disagree.

The archive is the following five-element array:

```text
[
  1,
  manifest_utf8_bytes,
  private_key_records,
  portability_records,
  inert_desktop_annotations
]
```

`manifest_utf8_bytes` contains the original bounded UTF-8 `.ftworkspace` document. Preserve its exact bytes for the export; validate JSON with duplicate-key rejection before using it. There is no requirement to reinterpret JSON through a second platform-specific canonicalizer.

Each private record is `[credential_id_text, 1, private_container_bytes, public_key_blob_bytes, user_certificate_blobs]`. Format `1` is the single-key, clear OpenSSH container described above. Public blobs and certificates use their actual supported SSH encodings, not paths or inferred names. Reject duplicate credential IDs, mismatched identities, missing required references, and unexpected additional private records.

Each portability record is `[entity_id_text, setting_name_text, disposition_uint, reason_text]`, where disposition `0` means preserved but inactive and `1` means a required unresolved condition. Each inert annotation is `[entity_id_text, setting_name_text, original_value_utf8_bytes]`. Neither record can supply executable instructions to the importer. A required unresolved condition must appear in the preview and corresponding route readiness.

All record counts, text lengths, key sizes, and aggregate decoded bytes are checked before expensive work. This archive is not a ZIP/TAR filesystem extraction operation; there are no archive paths, symlinks, or filenames to traverse.

### 3.3 Plaintext packing and padding

Let `A` be the archive bytes. Choose payload `D=A` with codec `0`, or a single RFC 1950 zlib stream of `A` with codec `1` when it is smaller. No dictionary or concatenated stream is allowed. Do not compress ciphertext or use a content-dependent external compression dictionary.

The authenticated plaintext is:

```text
P = U8(codec) || U32BE(len(A)) || U32BE(len(D)) || D || zero_padding
```

Append the smallest number of zero bytes making `len(P)` a positive multiple of 4096. Thus `len(P) = 4096 * ceil((9 + len(D)) / 4096)`. Check overflow before rounding. The receiver requires exactly this padding length and all-zero padding. With codec `0`, both length fields must agree. With codec `1`, expansion must finish at exactly the declared archive length, consume exactly the declared compressed bytes, and stay within the absolute decoded-byte cap.

The padding obscures precise package size within a bucket, not the existence or approximate size of a transfer. No names, key fingerprints, route addresses, or counts appear outside encryption. Compress only the frozen, user-reviewed package; do not expose an automatic chosen-input compression oracle to remote terminal output.

### 3.4 FTAG fixed header

All lengths below are byte lengths; all multibyte integers are unsigned big-endian. FTAG v1 has one fixed 30-byte prefix:

| Offset | Bytes | Meaning |
|---:|---:|---|
| 0 | 4 | ASCII `FTAG`. |
| 4 | 1 | Envelope version `1`. |
| 5 | 1 | Mode `1` for generated-code encryption, `2` for recipient-bound HPKE. |
| 6 | 2 | Reserved flags, exactly zero. |
| 8 | 16 | Fresh random transfer ID. |
| 24 | 4 | Ciphertext length, including the 16-byte authentication tag. |
| 28 | 2 | Mode-header length, exactly 28 or 80 for the selected mode. |

`H` is this prefix followed by the selected mode header. The envelope is exactly `E = H || C`, with no trailing bytes. The actual length must equal `30 + mode_header_length + ciphertext_length` and fit the optical envelope cap. `ciphertext_length - 16` must be a positive multiple of 4096.

The additional authenticated data is the exact byte string:

```text
AAD = UTF8("FrankenTerm/FTAG/v1/envelope") || 0x00 || H
```

Reject unknown versions, modes, nonzero reserved flags, invalid lengths, and forbidden mode-header combinations before selecting a primitive or allocating based on them. Never fall back from a failed recipient mode to a weaker code mode.

Use maintained implementations of standard cryptography. These framing choices do not authorize hand-written ciphers, custom elliptic-curve arithmetic, or substitutions that happen to have similarly named APIs.

## 4. Code-protected one-way encryption

### 4.1 A random transfer code, not a weak password

Generate a fresh, uniformly random 16-byte secret `S` with the operating system's cryptographically secure random source. It supplies 128 bits of entropy. Do not derive it from a machine name, timestamp, six-digit PIN, ordinary user password, SSH passphrase, or visual comparison value.

Represent `S` as a 26-character, zero-left-padded base-32 integer using alphabet `0123456789ABCDEFGHJKMNPQRSTVWXYZ`. The first character is `0` through `7`. Append two characters representing the first ten bits of:

```text
SHA256(UTF8("FrankenTerm/FTAG/v1/code-check") || 0x00 || S)
```

Display the resulting 28 characters as seven groups of four. The two final characters detect typing mistakes; they add no secret entropy and are not an authentication tag. On entry, accept ASCII lowercase, remove ASCII spaces and hyphens, and normalize `O` to `0` and `I`/`L` to `1`. Reject other characters, wrong length, noncanonical top bits, and a checksum mismatch before key derivation. Re-render the canonical form for review.

The code is generated, never chosen. Therefore this profile uses HKDF, not a password KDF, and makes no claim that HKDF strengthens low-entropy passwords. A later human-password mode would need a separately versioned bounded password-KDF policy and qualification; it must not be smuggled into mode `1`.

### 4.2 Exact cryptographic profile

The mode header is `salt[16] || nonce[12]`, both generated afresh for each new envelope. Derive a 32-byte key with [HKDF-SHA-256, RFC 5869](https://www.rfc-editor.org/rfc/rfc5869.html):

```text
K = HKDF-SHA256(
  IKM = S,
  salt = salt,
  info = UTF8("FrankenTerm/FTAG/v1/code-key"),
  length = 32
)
C = ChaCha20-Poly1305.Seal(K, nonce, P, AAD)
```

Use the IETF 96-bit-nonce ChaCha20-Poly1305 profile from [RFC 8439](https://www.rfc-editor.org/rfc/rfc8439.html), producing ciphertext followed by its 16-byte tag. There is one seal operation per immutable envelope. Replaying a video transmits the same bytes; it does not reseal frames. Rebuilding, changing selected keys, or changing manifest content creates a new transfer ID, code, salt, and nonce.

Authenticate the entire plaintext before parsing its length fields, decompressing, interpreting JSON, or staging any key. An AEAD API that releases tentative plaintext before tag validation must keep that plaintext inaccessible to all consumers until final success.

### 4.3 Separate presentation of the secret

The Mac shows the transfer code in a dedicated reveal step before playback or while playback is paused behind an opaque cover. Hide it before displaying QR frames. The code is never included in the QR, video frames, audio, subtitles, thumbnail, filename, metadata, clipboard by default, diagnostic logs, or an exported sidecar.

Revealing the code again pauses and covers the animation. This reduces accidental combined capture; it does not defeat a camera recording the entire reveal-and-play session. Explain that limit plainly. A saved video and a saved copy of its code must be protected separately.

Closing the export releases owned plaintext/key buffers and the generated code according to the actual platform's memory guarantees. It does not cryptographically expire recordings. The word **one-time** describes generating a new code per export, not a promise that recorded ciphertext can be decrypted only once.

## 5. Recipient-bound optical encryption

### 5.1 A public request from the phone

For **Encrypt only for this phone**, the phone generates a fresh X25519 recipient key pair and a random 16-byte request ID. Its request is deterministic CBOR:

```text
R = [1, request_id_bytes16, recipient_public_key_bytes32, maximum_envelope_bytes]
```

`maximum_envelope_bytes` is at most the application's absolute cap. Encode the exact CBOR bytes as a CBOR byte string in a single-part `ur:bytes`, displayed with a distinct `FTREQ1:` prefix. The request is at most 4096 bytes. The Mac accepts it only through the explicit recipient-request scanner, never as a terminal command or general URL.

The Mac and phone show the first 20 hex characters of `SHA256(R)` for recipient confirmation before any private material is exported. A substituted request must not silently change the destination. The ephemeral recipient key is separate from SSH identities and the long-lived Via Mac device identity. Do not claim this software X25519 key is a Secure Enclave P-256 key.

### 5.2 Standard HPKE, not custom ECDH encryption

Use [RFC 9180 HPKE](https://www.rfc-editor.org/rfc/rfc9180.html) Base mode with the fixed suite:

| Component | Identifier |
|---|---|
| DHKEM(X25519, HKDF-SHA256) | `0x0020` |
| HKDF-SHA256 | `0x0001` |
| ChaCha20-Poly1305 | `0x0003` |

Set up the sender context using the confirmed recipient public key and:

```text
info = UTF8("FrankenTerm/FTAG/v1/recipient") || 0x00 || SHA256(R)
```

The 80-byte mode header is `request_id[16] || SHA256(R)[32] || enc[32]`, where `enc` is the HPKE encapsulated key obtained from sender setup. Form `H` and `AAD` after setup; then seal `P` once with that context. The receiver uses its matching retained request/private key, the received `enc`, identical `info`, and identical `AAD`. HPKE owns nonce derivation; this mode has no independently supplied AEAD nonce or generated-code fallback.

Qualify the selected native CryptoKit or Rust HPKE implementation against RFC vectors and cross-platform envelope fixtures. Apple's [HPKE API](https://developer.apple.com/documentation/cryptokit/hpke) is a candidate native surface, not proof that the exact selected suite and integration already pass those checks.

HPKE Base mode does **not** authenticate the sender. Anyone knowing the public recipient key can construct a new encrypted message. Confirm the completed envelope's visual comparison value against the trusted Mac and apply the existing exporter/trust review before activation. An encryption tag cannot replace that source-authentication step.

### 5.3 Lifecycle and backup limits

Retain only a bounded number of active recipient requests, initially one. Keep the ephemeral private key in memory for the current explicitly active receive session; cancellation destroys the owned request/key state. Device lock or backgrounding stops camera work and requires an explicit resume policy. Process death loses an unpersisted request key, so a newly generated request requires a newly encrypted export.

A deliberately implemented device-protected resume option may store the request key under a short-lived, device-only Keychain policy, but it must have its own cleanup and lifecycle tests before exposure. It must not be restored on a different phone by copying configuration.

Do not advertise a recipient-bound video as an independent disaster-recovery backup. Loss of the recipient private key can make it undecryptable. Offer code-protected mode for a recoverable video stored separately from its generated code. Deleting a private key prevents future local decryption only to the extent all copies and unlocked material were actually removed; no expiry field revokes a recording by itself.

## 6. QR framing and fountain recovery

### 6.1 Reuse an established multipart algorithm

Use the Blockchain Commons [Multipart UR architecture](https://developer.blockchaincommons.com/animated-qrs/) for fragmentation and Luby-transform fountain coding. Its published implementation guide and vectors define fragment choice, degree sampling, PRNG, CRC, padding, sequence numbering, and CBOR representation. Pin the exact reference revision during implementation and verify interoperability rather than inventing another incompatible fountain algorithm.

The first `N` multipart parts are systematic source fragments; later sequence numbers describe repair mixtures. A sequence number greater than `N` is normal, not an out-of-range error. Receipt of `N` arbitrary callbacks or even `N` distinct mixed symbols does not guarantee reconstruction; only enough independent information does. Duplicate and dependent symbols must not inflate progress.

The underlying UR objects are standard; the enclosing `FTQR1` session/page wrapper below is FrankenTerm-specific. Do not claim an unrelated UR scanner will directly import the full prefixed text. QR error correction, Bytewords/UR checksums, page digests, and envelope authentication each retain their actual distinct role.

### 6.2 Bounded pages instead of one enormous solver

Split the immutable envelope `E` into consecutive pages of at most 32768 bytes. Let `L=len(E)` and `Pcount=ceil(L/32768)`. Page `i` is exactly `E[i*32768 : min((i+1)*32768,L)]`.

For each page, encode one standard `ur:bytes` object whose CBOR payload is a byte string containing exactly that page. Multipart encoding uses a maximum source-fragment length of 256 bytes. The 32768-byte page plus its three-byte CBOR prefix requires at most 129 source fragments under the reference partitioning rule. A sufficiently small last page uses the standard single-part UR syntax, not a fictional multipart degeneration.

This caps the algebraic problem independently of total workspace size. Recover and hash-check each page separately, then place it at its checked offset. Reconstructing a page does not expose plaintext or authorize an import. All pages still belong to one whole-envelope authentication operation.

### 6.3 Exact textual wrapper

Each data QR contains this ASCII text:

```text
FTQR1/<transfer-id-hex>/<envelope-length>/<page-index>-<page-count>/<envelope-sha256-hex>/<page-sha256-hex>/<UR>
```

Transfer ID is 32 uppercase hexadecimal characters. Both digests are 64 uppercase hexadecimal characters. Integers are canonical decimal with no leading zeros except `0`; page index is zero-based. `UR` is a complete uppercase standard single-part or multipart `UR:BYTES/...` string. Split only the first six `/` delimiters; slashes inside the UR belong to the UR parser.

Validate `page-count=ceil(envelope-length/32768)`, page range, exact reconstructed page length, integer overflow, and both digest lengths. After reconstruction, the wrapper's transfer ID must equal FTAG's authenticated header transfer ID. Its total length must equal the actual envelope length.

Lock the receiver to one explicitly selected `(transfer ID, envelope length, full envelope digest, framing version)` tuple. An unrelated QR in the camera view cannot reset or contaminate it. Within a page, require consistent message length, checksum, fragment geometry, and expected page digest. Bind all accepted results to that receive generation.

The prefix is not authenticated merely because it contains hashes. It prevents accidental mixing and supports repair localization; a malicious actor can still attempt solver poisoning or denial of service. Page SHA-256, full-envelope SHA-256, AEAD verification, bounded work, and deliberate source verification provide the remaining checks. Do not promote CRC-32 to an integrity or identity guarantee against an adversary.

### 6.4 Live scheduling and finite videos

The live sender cycles pages in short bursts, initially eight new UR parts per page per visit, rather than transmitting one page forever or requiring the receiver to see an introductory frame. Each page maintains its own sequence counter and continues generating new repair parts after its systematic phase. Periodically include systematic parts again to support simple recovery and difficult capture conditions.

Page order may rotate between rounds to reduce repeated starvation, but each QR remains self-describing. Dwell time and physical symbol size can change without changing the encrypted export, page layout, or fragment geometry. FTQR1 fixes the maximum fragment length; a future different fragmentation profile needs an explicit distinct profile/version, never an unannounced midstream change.

A saved video contains, for every page, all systematic parts plus an initial repair set, proposed as at least `ceil(N/2)+16` additional parts for multipart pages. Verify that all systematic data is present before marking video export successful. A finite video loops a finite set of symbols; replaying it does not generate new repair information. A phase-locked decoder can repeatedly miss the same symbols. Offer pause, slower playback, seeking, and live **Repair page** playback when the Mac still has the envelope. Never promise success from arbitrary permanent losses in a finite recording.

The phone can display page-level progress or the index of a stalled page. The user may select that page on the Mac for targeted fresh repair symbols. This is a manual optional feedback path, not a hidden network acknowledgement. In a one-way session the Mac knows only what it emitted, not what the phone received.

## 7. Mac display and exportable QR video

### 7.1 Legibility wins over theoretical QR capacity

Render high-contrast black modules on a white square, at an integer physical-pixel scale, without interpolation, gradients, decorative logos, or rounded modules. Keep the finder patterns and a minimum four-module quiet zone intact; see [DENSO WAVE's margin guidance](https://www.qrcode.com/en/howto/code.html/index.html). Place labels, transport state, and comparison values outside that quiet zone.

Choose and hold one QR version for the presentation profile based on the maximum encoded part, including the full FTQR1 wrapper, long sequence numbers, and correction level. Start with correction level Q and qualify M as a lower-density alternative. Increasing error correction is not automatically better if it makes the physical modules too small for the camera.

Use a proposed default of five distinct QR symbols per second, with explicit three-symbol and ten-symbol modes. Each symbol is a static hold, followed by a hard cut. There are no crossfades, rotations, scrolling grids, or symbol interpolation. The surrounding interface can be polished; the optical channel must be visually boring and stable.

Keep the source display awake only while the user has active playback open. Pause on lock or source-window teardown, hide the transfer code during playback, and avoid unsolicited screen-brightness changes. Provide size and brightness guidance without treating every failed decode as a cryptographic problem.

### 7.2 Save the stream, not a screen recording of secret UI

Generate exported video directly from already encrypted QR-frame images, never by capturing the application window. This prevents inclusion of the code-reveal step, key picker, authentication dialogs, another window, cursor movement, or notifications.

An MP4 profile can use a fixed 30 fps timeline, holding each symbol for six frames at the five-symbol default. Three-symbol mode uses ten frames; ten-symbol mode uses three. Use no audio track and no secret-bearing metadata. Qualify actual codec/bitrate/keyframe and range settings through encode/decode tests; selecting H.264 or a high nominal bitrate alone does not prove sharp QR edges survive.

The encoder must preserve every intended symbol interval and a complete finite symbol set. Decode the produced movie back into frames and verify their exact recovered FTQR1 payloads before finalizing it. Count distinct QR symbols separately from repeated video frames. Source images that decode before encoding do not establish the output movie is usable.

Provide an optional lossless image-sequence or lossless-animation representation for archival use, subject to native support, but keep the in-app live display and ordinary MP4 journey first-class. Playback software must disable frame interpolation. Exporting through messaging services that resize or transcode video is not a qualified transport path merely because the original video passed.

Use an explicit destination chosen by the user and an innocuous suggested filename. Never save to Photos or a cloud-synced location automatically. A thumbnail contains ciphertext symbols only. The final video must not contain the generated transfer code in any frame, stream, metadata item, subtitle, or sidecar.

### 7.3 Honest sender controls

Show **Package prepared**, encrypted byte size, page count, emitted-symbol count, playback speed, and **Waiting for you to finish on the phone**. Do not show a fake receiver progress bar. Pause/resume reuses the same envelope. A changed workspace or key selection starts a clearly new export.

If generation, disk space, cancellation, or video validation fails, preserve the useful original workspace and show that no valid completed video was produced. Finalize a saved video atomically only after its validation succeeds. An incomplete temporary movie cannot masquerade as a complete key backup.

## 8. Native iOS camera and decoder ownership

Use AVFoundation with a native SwiftUI/UIKit camera host. `AVCaptureMetadataOutput` emits QR observations; [AVMetadataMachineReadableCodeObject.stringValue](https://developer.apple.com/documentation/avfoundation/avmetadatamachinereadablecodeobject/stringvalue) is appropriate because FTQR1 deliberately encodes ASCII text. Do not assume arbitrary binary QR payloads always have a valid string representation, or that a Core Image descriptor's error-corrected bytes are already the application byte payload.

The camera owner serializes capture configuration, start/stop, permission changes, and interruption handling. Barcode callbacks do only bounded prefix/length checks and enqueue a small owned observation. Fountain work, hashing, decryption, key parsing, and archive validation occur off MainActor under explicit cancellation and byte/work budgets. UIKit preview and progress updates remain on MainActor.

Use one selected capture region. Ignore unrelated QR codes and never invoke a general URL handler. When several transfers appear, require explicit source selection before allocating a second receive state. Keep the selected transfer identity visible. Rate-limit invalid observations and duplicates without allowing a flood to starve useful parts.

The state machine is:

```text
idle -> camera permission -> source selected -> collecting ciphertext
     -> envelope reconstructed -> code/request available -> authenticated
     -> source/trust review -> archive validated -> import staged
     -> workspace committed -> restored offline
```

Code entry may occur before scanning; stages may overlap only where their dependencies permit it. **100% scanned** is not **restored**. Show recovered pages/bytes and a clearly labeled decoding estimate for a partial page. A checksum, reconstruction, wrong-code, authentication, archive-validation, or Keychain error gets its own accurate state.

Store no camera frames or video by default. Resumable scan state consists only of ciphertext fragments/pages plus bounded metadata. Persisting it is opt-in, device-protected, excluded from automatic export, and never colocated with the generated code or plaintext private archive. On restoration, treat saved fragment metadata as untrusted and revalidate it. Cancellation releases all receive-owned state; late callbacks carry a retired generation and cannot publish into a new transfer.

Camera interruption, backgrounding, rotation, and device lock pause collection without inventing success. On return, reacquire the camera and continue only the same selected envelope. In recipient mode, confirm the matching private request key still exists; otherwise explain that a fresh recipient request and new export are required.

Do not announce every scanned frame to VoiceOver or issue per-frame haptics. Announce meaningful progress milestones, completion, and actionable failures. The transfer code has an accessible grouped-entry control without automatically reading a private code aloud. Non-camera document import remains an alternate path to the same envelope verifier.

## 9. Safe restoration and offline operation

### 9.1 Network silence is a real mode

The optical import pipeline performs no DNS, HTTP, SSH, TLS pairing, Bonjour, Bluetooth, APNs registration, analytics upload, remote font fetch, online licensing, or other network bootstrap. Dependencies and camera/code support must already be installed in the application.

For an app-wide **Offline Restore** promise, acquire a process-wide network-inhibition scope before restoration begins. Existing terminal connections cannot simply continue in the background while the app claims network silence. If they are live, explain the consequence and offer an explicit choice to detach them or cancel entry into strict offline mode. Follow the parent plan's weaker direct-SSH survival rules; never silently kill a user's shell to make a demo air-gapped.

The app does not claim to control every radio or other process on the device. Qualify the journey with both devices' network interfaces disabled and verify zero application network attempts. Mac export similarly uses only local state. Persist the restored workspace's initial `connect_later` gate so a crash/relaunch after publication cannot bypass offline mode through imported auto-connect defaults.

### 9.2 Authenticate, validate, then publish

The complete order is:

1. Reconstruct every page at its bounded expected offset and verify each page digest.
2. Verify the full envelope digest, exact size, framing/header agreement, and mode bounds.
3. Obtain the correct generated code or matching recipient private request key.
4. Authenticate the full ciphertext and AAD; do not expose tentative plaintext.
5. Validate packing, exact padding, compression, decoded limits, CBOR shape, and manifest JSON.
6. Derive and compare each private/public/certificate identity, and reconcile references.
7. Apply source/trust review, omission review, existing revocations, and local merge policy.
8. Stage keys under an owned import transaction, publish one immutable workspace revision, and commit the import journal.
9. Release transient secrets and present the restored-but-offline result.

Keychain and workspace storage do not become one atomic database because they arrived in one QR animation. Reuse the parent section 9.4 transaction journal, ownership checks, staging identifiers, and crash recovery. Existing referenced keys are never deleted by failed-import cleanup. A partial Keychain failure cannot publish affected routes as ready.

No network test is necessary to commit offline configuration. Readiness describes locally validated configuration and credentials, not proof of remote reachability or permission. Unsupported mandatory security policy remains visibly blocked.

### 9.3 Replay and revocation

Persist accepted bundle IDs and envelope digests with the import revision. Re-scanning the same immutable envelope is idempotent. An equal transfer/bundle ID with different bytes is a conflict, not a replacement. A changed or older authenticated export produces an explicit merge/rollback review; an unsigned claimed sequence number cannot prove freshness.

Keep local revocation tombstones and trust restrictions independent of imported preferences. An old video cannot quietly resurrect a revoked phone authorization or clear a host-key rejection. Importing a copied SSH key preserves its original public identity, including the parent plan's warning that a server cannot distinguish or revoke just one physical copy of that same key.

Optical restore cannot create a new authorized entry in the Mac's gateway registry without a deliberate corresponding Mac operation. A saved gateway address or public certificate is not that operation. Preserve the route and indicate any remaining enrollment requirement.

## 10. Bounds, throughput, and failure behavior

### 10.1 Initial resource profile

These are proposed admission ceilings to qualify on the minimum supported device, not observed limits or preallocated amounts:

| Resource | Initial cap |
|---|---:|
| Full encrypted FTAG envelope | 4 MiB |
| Expanded CBOR archive | 8 MiB |
| Embedded public workspace JSON | Parent limit: 2 MiB |
| Private-key records | 256, deduplicated by canonical identity |
| One private container or certificate | 64 KiB |
| Certificate count / aggregate certificate bytes | 256 / 1 MiB |
| Individual display/reason text | 4096 UTF-8 bytes |
| CBOR nesting | 12 |
| Transport page | 32768 bytes |
| Number of pages | 128 |
| UR source fragments per page | 129 |
| One captured FTQR1 string | 2048 ASCII bytes |
| Pending camera observations | 8; duplicates coalesced |
| Retained mixed equations per page | At most `2*N`, plus bounded solved fragments |
| Aggregate optical solver/storage working memory | 32 MiB, reserved before growth |
| Whole optical receive/import incremental memory | Initial target 96 MiB, including camera and decode transients |

The whole application's footprint still matters. Evict optional terminal previews and release idle GPU work before a large optical import; do not add 96 MiB on top of an already exhausted phone budget. Count camera buffers, image conversion, solver containers, encrypted assembly, authenticated plaintext, decompression, parsed objects, and Keychain staging copies rather than only the final file.

Use checked arithmetic and reserved capacity throughout. Per-page solver state must use bounded index sets/bitsets and release redundant equations. Do not retain every observed frame in a dictionary keyed by an ever-increasing sequence number. Sequence counters follow the underlying UR's checked integer range and never wrap to an old identity.

Bound work per decode slice and expose an explicit resource-failure result. A malicious source can deny progress, but it must not monopolize MainActor or force unlimited work. If the reference solver would exceed an admitted resource budget, fail or reset only the affected incomplete page with a visible explanation, preserving already hash-verified pages. Do not silently alter the reference fragment-choice algorithm to pretend compliance.

Larger packages are refused before optical playback with their actual size and a deliberate group/identity-selection or ordinary encrypted-file alternative. Never truncate a key list, silently drop settings, or market the size cap as unlimited. Support for a larger profile is separate work with corresponding resource and optical-time qualification.

### 10.2 Throughput is measured useful data

Report useful recovered envelope bytes per second, not camera fps, encoded-video fps, scanned-character count, or theoretical QR capacity. Let `B` be encrypted bytes, `s` average useful fragment bytes, `f` distinct displayed symbols/second, `p` usable capture fraction, and `a` observed redundancy/duplicate/dependency overhead. A planning estimate is `T ≈ B*a/(s*f*p)`, plus setup and import costs. It is not a guaranteed fountain convergence bound.

For example, **64 KiB, 250 useful bytes/symbol, five symbols/second, 80% useful capture, and overhead 1.5 imply about 98 seconds** of collection. This is an illustrative arithmetic scenario, not measured device performance. A multi-megabyte export can take many minutes and should be identified before playback. Small, deduplicated, compressed machine groups are the primary optical workload.

The Mac may show a conservative estimate from encoded size and the selected profile. Only the phone can refine it using observed useful recovery. A stalled page must not produce an endlessly decreasing fake ETA.

### 10.3 Fail safely at the correct stage

| Failure | Response |
|---|---|
| Blur, glare, or a missed frame | Ignore the unreadable observation; continue collecting repair information. |
| Duplicate or dependent fragment | Do not increase recovered-data progress. |
| Unrelated transfer in view | Ignore or offer deliberate source change; retain the selected transfer. |
| Inconsistent fragment geometry or failed page hash | Reject affected data; bounded page repair/reset, no key import. |
| Whole-envelope digest or AEAD failure | No plaintext consumers or Keychain writes; diagnose code mismatch separately where possible without inventing certainty. |
| Unsupported required version, algorithm, key family, or policy | Explicit refusal or reviewed inactive route; no downgrade. |
| Corrupt/compressed bomb, excessive key, or oversized document | Reject at the earliest checked boundary. |
| Process death during import | Parent transaction journal recovers old or committed new configuration. |
| Recipient key unavailable | New request/new export, not a fallback to decrypting as code mode. |
| Lost saved-video code | Explain that this encryption mode has no server recovery mechanism. |

## 11. Architecture and implementation sequence

### 11.1 Ownership and reuse

Keep the UI thin: SwiftUI owns the export/import screens, AVFoundation owns camera capture, the native credential broker owns access to platform secrets, and shared portable code owns manifest validation, archive/envelope framing, page assembly, limits, and import intent. Native crypto may sit behind an exact primitive contract; cross-language implementations must consume the same vectors.

The optical subsystem must not depend on live SSH or mux discovery to perform its work. It consumes the parent exporter's typed snapshot and produces the parent importer's validated candidate. It does not create a second configuration model or bypass the existing credential broker.

Reuse the canonical Rust runtime surface and existing audited native boundaries. Evaluate the UR reference implementation and dependencies before adopting or narrowly implementing its published consensus stack. Do not add a second Rust runtime or a C++ QR/fountain library casually. Platform QR APIs are acceptable when their actual text mode, density, and exact-byte behavior pass the profile tests.

### 11.2 Meaningful delivery slices

| Slice | Complete capability |
|---|---|
| **OPT-A: Authenticated offline package** | Typed snapshot plus selected synthetic identities round-trip through the actual FTAG encoder/decoder; corruption, wrong code, and identity mismatch are rejected before import. |
| **OPT-B: Real screen-to-camera transfer** | A Mac display sends an encrypted representative workspace to the iOS app, surviving dropped/reordered frames and interruption with both devices offline. |
| **OPT-C: Real settings and Keychain restore** | Selected existing exportable identities and machine themes restore transactionally, preserving revocation/merge rules and remaining offline after crash/relaunch. |
| **OPT-D: Saved video and repair UX** | An exported movie replays successfully through real cameras, with validated complete symbol coverage, slow mode, and targeted live page repair. |
| **OPT-E: Recipient-bound mode** | Optical recipient request, confirmed HPKE destination, passive-recording resistance, source review, and lifecycle behavior pass on the packaged native build. |

Code-protected transfer, real camera recovery, and transactional restoration are the core product. Recipient mode and elaborate archival presentations must not postpone that complete useful slice. M0 direct-SSH functionality remains an independent native risk-reduction track; optical import extends IOS-B and its M1 Mac-to-phone journey.

These `OPT-*` labels are planning identifiers, not created or closed Beads. When implementation tasks are created, attach them to the existing exporter, importer, credential, and native iOS owners rather than duplicating server continuity work. This document does not close any implementation bead.

## 12. Acceptance and adversarial qualification

### 12.1 Deterministic protocol tests

Retain cross-platform fixtures for the generated code and typo checksum, HKDF output, complete header/AAD bytes, padding, ciphertext/tag, envelope digest, page boundaries, and inner UR encoding. Use public synthetic material only. Run the actual selected crypto implementation against the RFC's independent test vectors as well as application vectors; self-round-tripping alone can hide matching encoder/decoder defects.

Test every one-byte mutation in each header region, wrong mode-header length, unknown flags, wrong code, wrong recipient/request, ciphertext truncation, appended data, tag mutation, duplicate JSON keys, malformed CBOR, excess lengths, forbidden archive nesting, corrupted private/public binding, and duplicate credential references. Assert that no publication or Keychain staging sink is reached on rejection.

For fountain transport, test starting at arbitrary points, reordered observations, duplicates, random loss, long bursts of loss, dependent repair symbols, part numbers beyond `N`, single-part last pages, final-page lengths around boundaries, matching CRC with mismatched page content, competing transfer IDs, and adversarial repeated equations. Verify exact recovered envelope bytes, not only a decoder's success flag.

The finite-video test must establish complete systematic coverage, then simulate losses over repeated loops. Distinguish successful reconstruction from permitted inability to converge under permanent correlated loss. Recovery controls are part of the product, not a reason to weaken the success assertion.

### 12.2 Physical-device tests

Qualify a smaller/older supported phone and a current phone against representative Mac displays, scaled Retina modes, LCD/OLED monitors where available, brightness levels, viewing angles, camera distances, mild glare, motion, and camera interruption. Test the actual native QR decoder, not just desktop decoding of pristine images.

Use synthetic workspaces approximating twenty machines and multiple key families. Exercise different palettes and keyboard mappings, required unsupported policies, reused identities, existing local overrides, an intentionally revoked key, and a key that cannot be exported.

Disable network interfaces on both endpoints for the offline collection/restore journey. Independently verify zero application network attempts, no camera/video persistence by default, no plaintext secret in video or logs, and no auto-connect after import or a crash/relaunch. After the user explicitly leaves offline mode, connection qualification is a separate SSH test.

Crash at every parent import-journal boundary. Prove that the old or committed new workspace survives and that cleanup touches only transaction-owned unreferenced staging items. A successful scan with orphaned private keys or a partially published trust policy is not a pass.

### 12.3 Decisive security distinctions

A test with only the ciphertext recording must not decrypt code mode without `S`. A passive recording of the recipient request and response must not decrypt HPKE mode without the recipient private key. These tests demonstrate the intended data dependencies, not a novel cryptographic security proof.

Also test the counterexamples: possession of both code and recording permits decryption; HPKE Base encryption can be constructed by a third party holding the public recipient key; stale videos can still decrypt; nonexportable keys do not magically migrate; process termination may destroy an ephemeral recipient key. The UI and documentation must agree with those outcomes.

Do not claim a native build, camera qualification, penetration test, App Store approval, or end-to-end implementation from a protocol document or a generated fixture alone. Native proof and DSR release requirements remain those of the parent plan and repository instructions.

## 13. Parent-plan integration and primary references

### 13.1 Explicit integration map

| Parent area | Extension supplied here |
|---|---|
| Sections 0 and 2: first-use experience | Offline QR Transfer and Restore from Animated QR become first-class alternatives to live pairing. |
| Section 8: Mac export | Consistent complete snapshot, selected-key inclusion, completeness preview, immutable encrypted envelope, live/video output. |
| Sections 9.1 and 10.4: previous offline deferral | Superseded for FTAG envelopes only; the raw `.ftworkspace` JSON remains private-key-free and live TLS pairing remains supported. |
| Sections 9.3-9.4: import | Same merge and crash-safe publication, preceded by whole-envelope authentication and key-identity verification. |
| Sections 10.3, 10.6-10.8: credential and trust authority | No weakened key policy, no stale-video revocation bypass, no automatic gateway enrollment, no false source identity from a checksum. |
| Sections 12 and 21: lifecycle/privacy | Generation-bound scanner resume, protected optional ciphertext cache, explicit ephemeral-key policy, persistent Connect Later gate. |
| Sections 18 and 23: resources/performance | Optical solver/camera/import accounting and measured useful transfer throughput. |
| Section 22: native permissions/release | Camera permission for scanning; no new network entitlement or service is required by optical transfer. |
| Sections 24-25: acceptance and implementation | OPT-A through OPT-E extend IOS-B/M1 while preserving independent device and mux qualification. |

This root companion is the authoritative optical-transfer extension. The earlier parent prohibition was a scope boundary, not a security principle requiring a network; it remains applicable to unencrypted raw manifests and any offline secret format that has not implemented this contract.

### 13.2 Primary references

- [Blockchain Commons Animated QRs and its linked multipart implementation guide/reference implementations](https://developer.blockchaincommons.com/animated-qrs/): established systematic/rateless UR encoding and recovery behavior. Pin exact implementation/vector revisions before shipping.
- [RFC 8949](https://www.rfc-editor.org/rfc/rfc8949.html): deterministic bounded CBOR representations.
- [RFC 5869](https://www.rfc-editor.org/rfc/rfc5869.html): HKDF, used with a generated high-entropy secret rather than a human password.
- [RFC 8439](https://www.rfc-editor.org/rfc/rfc8439.html): IETF ChaCha20-Poly1305 primitive and independent vectors.
- [RFC 9180](https://www.rfc-editor.org/rfc/rfc9180.html): HPKE suite composition, Base mode, associated data, and sender-authentication limits.
- [Apple CryptoKit HPKE](https://developer.apple.com/documentation/cryptokit/hpke): candidate native HPKE integration.
- [Apple machine-readable QR observations](https://developer.apple.com/documentation/avfoundation/avmetadatamachinereadablecodeobject) and [stringValue](https://developer.apple.com/documentation/avfoundation/avmetadatamachinereadablecodeobject/stringvalue): native camera decoding and text-versus-binary distinction.
- [DENSO WAVE QR margins](https://www.qrcode.com/en/howto/code.html/index.html): four-module quiet-zone requirement.
- [Apple Secure Enclave](https://developer.apple.com/documentation/security/protecting-keys-with-the-secure-enclave) and [device-only passcode-bound Keychain access](https://developer.apple.com/documentation/security/ksecattraccessiblewhenpasscodesetthisdeviceonly): nonexportable identity and local credential storage constraints.

The FTAG/FTQR framing, caps, interaction defaults, packaging choices, and acceptance workloads are proposed FrankenTerm design decisions. They are not claims that these exact application profiles are external standards or already implemented.
