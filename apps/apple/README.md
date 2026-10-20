# Native Apple implementation

This directory contains the shared Swift 6 `FrankenTermKit` package and the native Xcode project for the first offline workspace-transfer slice. It is implementation source, not a signed or device-qualified terminal release.

## Entry points

Open `apps/apple/FrankenTerm.xcodeproj` with a stable Xcode supporting the iOS 26 SDK and Swift 6. The shared `FrankenTerm` scheme builds the iPhone/iPad application; `FrankenTermExport` builds the macOS companion (macOS 14 deployment floor). Both link the local package. There are no remote package downloads, generated project scripts, or build-time network dependencies. Development device signing needs the operator's selected team; no team or signing secret is embedded here.

The iOS app provides a native camera scanner, explicit transfer selection, bounded observation admission, authenticated package review, transaction-owned Keychain import, encrypted-file import, and a machine/group browser preserving colors and ordering. Review and saved-workspace screens report credential availability, required policies, host rejection, and dependent jump-route requirements from protected storage. It is single-scene until terminal/session ownership is integrated. It does **not** start SSH sessions, render a pretend shell, or execute imported auto-connect preferences.

The Mac companion opens a **preexisting local `.ftworkspace` JSON document**, lets the user select a machine group, and previews its complete route, jump-host, trust, theme, and credential dependencies. It accepts individually selected matching **unencrypted OpenSSH Ed25519 and ECDSA P-256/P-384/P-521 key files**. Keys can be excluded again; only identities needed by the selected group enter the encrypted archive. Selection changes invalidate the previous approval, code, and QR presentation.

It offers live fountain-coded QR playback with periodic source replay, targeted page repair, 3/5/10-symbol-per-second timing, separately revealed transfer codes, encrypted `.ftairgap` saving, and saved **lossless APNG** animation. The phone lists unfinished page numbers matching the Mac's Repair menu and can restart one unfinished page while retaining verified pages and the selected source. The companion does not yet capture a running desktop FrankenTerm workspace automatically. Encrypted key files, RSA, certificates, executable configuration, and recipient-bound encryption remain unsupported.

## Implemented invariants

- `WorkspaceStore` serializes the journal, Keychain ownership, and catalog publication under an exclusive persistence lease. Before deleting staged or retired identities, recovery reaffirms the selected catalog's **durability**. Reading renamed bytes after a failed fsync alone does not authorize deletion.
- Import confirmation is tied to an exact decrypted candidate and previously reviewed saved revision. Existing local revocation denies resurrection from an old recording. Missing device keys can be restored from the same backup without duplicating surviving keys.
- Accepted bundle, transfer, and envelope identities survive replacement in bounded history without silent eviction. A previously accepted recording that replaces the current workspace requires separate historical-rollback approval. Every newly prepared Mac archive receives a fresh bundle ID; playback and saved media reuse that archive's exact ciphertext.
- Host-key rejection persists by canonical host and port, independently of imported record IDs. DNS/IDNA, IPv6, and mapped IPv4 spellings cannot remove it. Store-backed verification evaluates negative trust before positive pins, including a presented certificate authority; certificates themselves still require a future qualified implementation. Every jump hop contributes its own readiness requirements.
- Credential reuse binds both the canonical public fingerprint and algorithm. An import without private material cannot relabel an existing Keychain identity. Matching aliases share material while retaining route-specific candidate order and authentication policy.
- The published catalog contains public manifest bytes and account references, not private containers. `connectLater` remains persisted through restart. Keychain items are device-only, passcode-bound, user-presence protected, nonsynchronizable, and created without overwriting existing accounts.
- Portability records must reference existing entities. Required unresolved conditions block their affected routes and dependent hops. Only registered boolean/opacity desktop annotations may enter public storage; unknown optional opaque values are discarded with a content-free finding, and unsupported required annotation values reject. Version-one and version-two local catalogs migrate durably to version three, including the original singular-credential schema; the network import parser continues to require the current manifest format.
- `OfflineRestore` distinguishes ciphertext reconstruction, authentication, review, commit, and recovery. Wrong-code retries preserve ciphertext. Suspension discards decrypted previews and retires observation generations. An uncertain publication is reconciled, not blindly repeated.
- A QR observation cannot silently change the selected source. Eight bounded strings feed one consumer; there is no task-per-camera-frame fanout. The scanner does not open QR URLs or retain camera images.
- Restarting an unfinished QR page clears only that page's equations and expected digest, preserves all other progress, and retires the old receive generation. Queued observations cannot repopulate it under stale authority.
- The Mac removes the QR surface and invalidates pending raster results before revealing the code. The animation encoder accepts **only ciphertext**, not an application-window screenshot or a transfer code.
- Saved recordings contain every systematic symbol and a repair set. Each APNG frame, timing, loop metadata, and reconstructed envelope are verified before its final name is published. Existing destinations are not overwritten. Larger recordings are rejected before rendering rather than truncated; the initial saved path caps 256 frames, 1024-square raster input, and 128 MiB output. Live transfer remains available for larger bounded envelopes.

## Verification status and commands

The complete shared package's **111 Linux XCTest cases passed** with Swift 6.2.1 after integration. Coverage includes the committed cryptographic and multipart-UR vectors, all 8,340 single-byte FTAG corruptions, all three ECDSA curves and adversarial key containers, metadata reference/content boundaries, exact numeric preservation during selected-group export, persistent negative trust, identity-alias reuse, true legacy-catalog migration, fault-injected journal/catalog recovery, late receivers, and selective QR-page restart. Both application Swift sources also passed syntax parsing. No compiler warnings or test failures were reported in the final portable run.

```sh
swift test --package-path apps/apple/FrankenTermKit -j 2
```

This Linux command uses OpenSSL only as the package's independent test oracle. Apple products use CryptoKit and never link that test-only target. `AppleOpticalPresentationTests` is intentionally Apple-only; its native QR and saved-animation test was **not** included in the Linux result.

The Xcode project and Info.plist passed structural parsing; both application Swift files passed syntax parsing. **No Apple SDK type-check, native app build, Keychain/device test, camera capture, APNG output run, or performance qualification was available in this session.** Those are still required. A parser pass or Linux conditional build does not establish that the Apple-only branches compile. Run the native package tests and both app schemes on the repository's authorized Mac build environment, then exercise a passcode-enabled iPhone with the debugger detached for lifecycle tests.

Release distribution must use the repository's DSR system after its native iOS lane is implemented. Development Rust work remains subject to RCH; this Swift slice introduces no Cargo build step or fallback.

## Critical continuation

The next terminal slice is the actual iOS-buildable Rust terminal/renderer bridge, bounded SSH driver, native text input, exact source/control ownership, and real mux attachment. The Mac's typed live-workspace snapshot needs to feed the same export engine, replacing manual manifest selection. Encrypted OpenSSH import, RSA and certificate qualification, HPKE recipient mode, and verified MP4 export remain explicit capabilities to implement.

Native qualification must specifically verify Keychain metadata-only queries under the chosen ACL, protected-storage interruption, CryptoKit ECDSA behavior, source selection and page repair during camera loss, code concealment during app deactivation, current Image I/O APNG behavior, symbol density and fixed-version QR requirements, and physical display/camera decoding at every offered speed. Device and SDK evidence must not be inferred from the 111 portable tests.
