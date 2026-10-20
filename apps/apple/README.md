# Native Apple implementation

This directory contains the shared Swift 6 `FrankenTermKit` package and the native Xcode project for the first offline workspace-transfer slice. It is implementation source, not a signed or device-qualified terminal release.

## Entry points

Open `apps/apple/FrankenTerm.xcodeproj` with a stable Xcode supporting the iOS 26 SDK and Swift 6. The shared `FrankenTerm` scheme builds the iPhone/iPad application; `FrankenTermExport` builds the macOS companion (macOS 14 deployment floor). Both link the local package. There are no remote package downloads, generated project scripts, or build-time network dependencies. Development device signing needs the operator's selected team; no team or signing secret is embedded here.

The iOS app provides a native camera scanner, explicit transfer selection, bounded observation admission, authenticated package review, transaction-owned Keychain import, encrypted-file import, and a machine/group browser preserving colors and ordering. It is single-scene until terminal/session ownership is integrated. It does **not** start SSH sessions, render a pretend shell, or execute imported auto-connect preferences.

The Mac companion opens a **preexisting local `.ftworkspace` JSON document**, accepts individually selected matching **unencrypted OpenSSH Ed25519 key files**, and encrypts the resulting archive. It offers live fountain-coded QR playback, targeted page repair, 3/5/10-symbol-per-second timing, separately revealed transfer codes, encrypted `.ftairgap` saving, and saved **lossless APNG** animation. It does not yet capture a running desktop FrankenTerm workspace automatically. Unsupported encrypted key files, key algorithms, certificates, executable configuration, and recipient-bound encryption are not silently approximated.

## Implemented invariants

- `WorkspaceStore` serializes the journal, Keychain ownership, and catalog publication under an exclusive persistence lease. Before deleting staged or retired identities, recovery reaffirms the selected catalog's **durability**. Reading renamed bytes after a failed fsync alone does not authorize deletion.
- Import confirmation is tied to an exact decrypted candidate and previously reviewed saved revision. Existing local revocation denies resurrection from an old recording. Missing device keys can be restored from the same backup without duplicating surviving keys.
- The published catalog contains public manifest bytes and account references, not private containers. `connectLater` remains persisted through restart. Keychain items are device-only, passcode-bound, user-presence protected, nonsynchronizable, and created without overwriting existing accounts.
- `OfflineRestore` distinguishes ciphertext reconstruction, authentication, review, commit, and recovery. Wrong-code retries preserve ciphertext. Suspension discards decrypted previews and retires observation generations. An uncertain publication is reconciled, not blindly repeated.
- A QR observation cannot silently change the selected source. Eight bounded strings feed one consumer; there is no task-per-camera-frame fanout. The scanner does not open QR URLs or retain camera images.
- The Mac removes the QR surface and invalidates pending raster results before revealing the code. The animation encoder accepts **only ciphertext**, not an application-window screenshot or a transfer code.
- Saved recordings contain every systematic symbol and a repair set. Each APNG frame, timing, loop metadata, and reconstructed envelope are verified before its final name is published. Existing destinations are not overwritten. Larger recordings are rejected before rendering rather than truncated; the initial saved path caps 256 frames, 1024-square raster input, and 128 MiB output. Live transfer remains available for larger bounded envelopes.

## Verification status and commands

The shared package's **41 Linux XCTest cases passed** with Swift 6.2.1 during the implementation session. This includes the previously committed cryptographic and multipart-UR vectors, all 8,340 single-byte FTAG corruptions, repair-only reconstruction, 13 fault-injected persistence tests, 8 restore-coordinator scenarios, and 3 finite-recording/local-file/group-selection tests. The actual existing source/test blobs were checked before the run.

```sh
swift test --package-path apps/apple/FrankenTermKit -j 2
```

This Linux command uses OpenSSL only as the package's independent test oracle. Apple products use CryptoKit and never link that test-only target. `AppleOpticalPresentationTests` is intentionally Apple-only; its native QR and saved-animation test was **not** included in the Linux result.

The Xcode project and Info.plist passed structural parsing; both application Swift files passed syntax parsing. **No Apple SDK type-check, native app build, Keychain/device test, camera capture, APNG output run, or performance qualification was available in this session.** Those are still required. A parser pass or Linux conditional build does not establish that the Apple-only branches compile. Run the native package tests and both app schemes on the repository's authorized Mac build environment, then exercise a passcode-enabled iPhone with the debugger detached for lifecycle tests.

Release distribution must use the repository's DSR system after its native iOS lane is implemented. Development Rust work remains subject to RCH; this Swift slice introduces no Cargo build step or fallback.

## Critical continuation

The next terminal slice is the actual iOS-buildable Rust terminal/renderer bridge, bounded SSH driver, native text input, exact source/control ownership, and real mux attachment. The Mac's typed live-workspace snapshot needs to feed the same export engine, replacing manual manifest selection. Encrypted OpenSSH import, additional key families/certificates, HPKE recipient mode, and verified MP4 export remain explicit capabilities to implement.

Native qualification must specifically verify Keychain metadata-only queries under the chosen ACL, protected-storage interruption, source selection during camera loss, code concealment during app deactivation, current Image I/O APNG behavior, symbol density and fixed-version QR requirements, and physical display/camera decoding at every offered speed. Device and SDK evidence must not be inferred from the 41 portable tests.
