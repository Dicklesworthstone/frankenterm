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

The format bounds a sealed segment (v5) at 4096 rows and 256 KiB of row
payload, and readers accept any segment within them. The store's writer
cuts segments at 1024 rows and 128 KiB, to bound what a cold read
decrypts (see Cold-read cost). A single row larger than that is sealed as a
segment of its own, up to the 16 MiB row limit. A segment never spans two
batches or windows.

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
- Across cold reads, the sink keeps the last segment it opened, so a
  sweep in Screen's ranged loads of at most 32 rows (the cold index,
  search) also opens each segment once. It reuses that segment only under
  the ledger, content epoch and first stable row it was read under, and
  only while the segment's tail record (whose tag covers the whole
  segment) is still stored unchanged at its sequence. Stored rows are
  rewritten only after a cut through the ledger's end and sealed again
  under a fresh nonce, so a rewritten segment never keeps its tail. A
  segment changed on disk after it was opened is detected when it is next
  opened; the kept rows were authenticated when it was opened.
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
rows; under 1024 with the writer's segments), and compaction keeps them,
so the segment still authenticates.
They never read back as rows: every reader refuses rows before the oldest
retained row. A tampered slack row makes the segment fail to open, which
fails closed as above.

## Plaintext hygiene

- The writer's reusable encode buffers are wiped after every row and
  segment (the bytes they held; their spare capacity never holds
  plaintext), and on drop.
- Decoders wipe their buffers on failure. Plaintext buffers are
  `Zeroizing` and wiped on drop.
- The segment a sink keeps open between cold reads holds at most one
  bounded segment's plaintext (256 KiB; a single larger row is not kept,
  and the buffer kept is at most 1 MiB). It is wiped when it is replaced,
  when a read fails, by a clear or a prefix replacement (so rows they
  remove leave no plaintext behind), and when the sink is dropped.

## Cold-read cost

Measured with `benches/scrollback_cold_read.rs` (mux-server-impl) on RCH
worker ts1, a shared Linux host, so a development signal, not an SLO.
Each shape is a live store holding row 0 and one full 4096-row commit
window, and the probes read the first and the tail row of its largest
segment. "Cold" means no segment is open in the process; the OS page
cache is warm.

With the writer's segments (1024 rows, 128 KiB):

| | t0_corpus | ascii91 | ascii9 |
| --- | --- | --- | --- |
| Largest segment | 285 rows, 131,071 B | 658 rows, 130,942 B | 1024 rows, 35,840 B |
| One cold row read (first / tail row) | 714 / 732 µs | 753 / 759 µs | 617 / 622 µs |
| The same read with its segment kept open | 310 / 314 µs | 310 / 314 µs | 316 / 313 µs |
| Cold 24-row viewport (first / tail row) | 865 µs / 1.18 ms | 841 µs / 1.20 ms | 638 / 829 µs |
| AEAD and decode: the whole segment, vs one v4 row alone | 213 vs 6.5 µs | 215 vs 5.9 µs | 77 vs 2.2 µs |
| All 4097 rows in 32-row loads: segment kept / every load cold | 72.4 / 127.6 ms | 60.7 / 122.6 ms | 43.8 / 85.3 ms |

- A single cold row read takes 0.62 to 0.76 ms. About 0.31 ms of every
  read, with a segment kept open or not, is the sink's per-call
  verification of the published manifest, which predates segment sealing
  (ft-mfip6). The segment's own share is 0.30 to 0.45 ms.
- A viewport that starts at a segment's tail row reaches into the next
  segment and opens both: 1.18 to 1.20 ms.
- With segments at the format's bounds (4096 rows, 256 KiB), and with a
  tail row's segment fetched twice, the same run measured single cold row
  reads of 0.97 to 1.53 ms (t0_corpus 971 µs / 1.18 ms, ascii91 1.03 /
  1.31 ms, ascii9 1.14 / 1.53 ms). That is why the writer cuts smaller
  segments, and why the reader now keeps the rows its scan back reads.
- The store no longer writes v4 rows, so a per-row store read cannot be
  measured for comparison; at the cipher, one v4 row opens in 2.2 to
  6.5 µs.

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
- `cold_reads_keep_the_last_segment_open_until_it_changes`: a sweep opens
  each segment once; a kept segment is refused once its tail changes or
  is gone, and is never reused across a clear that reuses its sequences.
  `cold_read_bench_hooks_read_back_exact_rows_from_the_stores_segments`
  (mux-server-impl).
- `pruned_tail_rows_stay_readable_until_a_compaction_drops_them`,
  `the_pruned_tail_is_bounded` (frankenterm-core).
