# Durable scrollback row sealing

Security note for ft-yccm0.2.1.4: how the durable scrollback store
encrypts and authenticates the rows it keeps, and what happens when they
fail to open.

## Row formats

Every row is one line of the pane's ledger. The ledger is addressed by
sequence: row `n` of a lineage is ledger line `n`.

| Prefix | Written by | Sealing |
| --- | --- | --- |
| `ftsl3e:` | the first row of a lineage, replacement ledgers, and batches while the nonce segment table is full | One XChaCha20-Poly1305 per row, under a 96-byte self-describing header (key id, pane, content epoch, revision, stable row, sequence, length, random 24-byte nonce) |
| `ftsl4e:` | no longer written; still read | One XChaCha20-Poly1305 per row, no header; the nonce is the nonce segment's base followed by the row's sequence |
| `ftsl5r:` / `ftsl5t:` | every other batch and commit window | One XChaCha20-Poly1305 per sealed segment; each row stores only its ciphertext slice, and the segment's last row (the tail, `ftsl5t:`) also stores the tag |

A sealed segment (v5) holds at most 4096 rows and at most 256 KiB of row
payload. A single row larger than that is sealed as a segment of its own,
up to the 16 MiB row limit. A segment never spans two batches or windows.

## Keys

Rows are sealed with the guardian output keyring's latest active key.
The key id of a nonce segment is stored once, in the authenticated
manifest's segment table and in the append WAL that starts the segment.
Historical keys stay available to open older rows; a key rotation starts
a new nonce segment.

## Nonces

- A nonce segment's 16-byte base is drawn from OS entropy when a process
  first writes a lineage, and again after a key rotation, a clear, a
  replacement ledger, or a batch cut back after a failure. An all-zero
  base is never used.
- A new base must differ from every base the ledger still retains. A
  collision means the entropy source repeats itself, and sealing is
  refused.
- The nonce of a v4 row is base || le64(sequence); the nonce of a v5
  segment is base || le64(first sequence). The in-memory stream that seals
  them only moves forward: sealing consumes every sequence of the row or
  segment, even if sealing then fails, and the stream refuses any sequence
  it has passed. A crashed process loses its stream; the next process
  draws a new base. So no (key, nonce) pair is ever used twice.

## What the tag binds

The AEAD's associated data binds, for a v5 segment: its own domain and
format version, the key id, the pane, the content epoch, the first row's
stable row and sequence, the row count, and every row's plaintext length.
For a v4 row it binds the same location for that one row. A row moved to
another pane, epoch, stable row or sequence, opened under another nonce
segment or key, or (for v5) reordered, dropped, duplicated, re-split
across a boundary, or marked as a tail in the wrong place, fails the tag.

## Verification and failure behavior

- Opening verifies the tag before decrypting. No unauthenticated
  plaintext is ever returned. On any failure the opener wipes its output
  buffers and returns an error.
- Only canonical encodings open: strict base64 (no padding, no non-zero
  trailing bits), and each record kind only where it belongs (the tail
  last). The plaintext size comes from the records' lengths and is
  charged against the caller's budget before anything is decoded.
- A v5 row authenticates only together with its whole segment. Readers
  open the segment once and keep it open for the next row, so sequential
  readers (snapshots, export, recovery validation, ranged reads) decrypt
  each segment once.
- What a failure means to each reader: a cold read returns no row;
  snapshots and export fail; recovery validation refuses to open the
  ledger; recovery adoption of an unpublished tail stops at the first
  segment that does not open and cuts everything from there.
- A segment whose tail never reached the disk (a torn window) never
  authenticates, so recovery cuts it from its first row.

## Retention

Retention stays exact: the manifest's oldest row, row count, record bytes
and chain describe exactly the retained rows. When the oldest retained
row sits inside a segment whose earlier rows retention evicted, the store
keeps those evicted rows physically, as its pruned tail (at most 4095
rows), and compaction keeps them, so the segment still authenticates.
They never read back as rows: every reader refuses rows before the oldest
retained row. A tampered slack row makes the segment fail to open, which
fails closed as above.

## Plaintext hygiene

- The writer's reusable encode buffers are wiped after every row and
  segment (the bytes they held; their spare capacity never holds
  plaintext), and on drop.
- Decoders wipe their buffers on failure. Plaintext buffers are
  `Zeroizing` and wiped on drop.

## Tests

- `a_corrupted_segment_is_refused_without_panicking` (mux): targeted
  damage plus 4096 seeded single-character mutations; every changed
  segment is refused and none panics.
- `a_segment_refuses_a_malformed_shape_before_sealing`,
  `a_segment_seals_its_rows_once_and_opens_them_at_their_location` (mux).
- `clustered_row_decoder_refuses_malformed_rows_without_panicking`
  (mux-server-impl): the plaintext decoder under 4096 seeded mutations.
- `segments_keep_exact_retention_and_the_oldest_rows_segment_readable`,
  `a_torn_segment_is_cut_from_its_first_row`,
  `a_reordered_or_torn_tail_is_adopted_only_up_to_its_first_bad_segment`
  (mux-server-impl).
- `pruned_tail_rows_stay_readable_until_a_compaction_drops_them`,
  `the_pruned_tail_is_bounded` (frankenterm-core).
