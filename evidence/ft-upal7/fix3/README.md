# ft-upal7 fix 3: where a recovery capture's time goes, and two fixes

Native macOS (M-series, shared and loaded machine), sealed `release-perf` builds so `sample`
resolves symbols. Every run: `multi.sh <family> 100000 40000 50 130`, that is 50 guardian
panes each printing 40,000 numbered rows at 100,000-row retention, the mux killed once its
first root is published, then restored and every pane's rows compared. Runs set
`E2E_RUST_LOG=info,frankenterm_pty_guardian::checkpoint_timing=debug`, so the guardian logs
how long its checkpoint worker spends on each job; `summarize.sh <sandbox>` totals those per
operation, over the whole run (until the sandbox stopped, so a run that published a third
generation before then counts its work too) and from the mux restart on.

## Profile (`profile/`)

`sample` of the guardian (two windows) and of the mux server (first root, later root),
aggregated per thread by `sample_agg.py` (inclusive and self counts) and `sample_callers.py`
(`ev-fix3b-profile.sh` arguments: the thread is named by its header, `Thread_<id>` for the mux
capture thread). Both keep a method's qualified self (`<Mux>::run`), so methods of different
types never share a row. The raw `sample` files (8 MB) are not committed; the aggregates are.

- First root, mux capture thread: 66% waiting for pane parsers' checkpoints. Across the 50
  parse threads 90% of that work is `materialize_cold_history`: each pane read its whole store
  (40,000 rows) to keep the newest ~7,000 under the 1 MiB first-root cap.
- First root, guardian checkpoint worker: 41% idle (its job-queue `recv`, called from
  `checkpoint_worker` itself).
- Later, full-size roots (5.4 MB a pane), guardian worker: 21% rereading and hashing published
  catalog candidates (14% for replay pages, 7% for replay opens' descriptors), 9% in the stage
  census of each chunk upload and about 8% in the F_FULLFSYNC calls of each record write.
- Later, full-size roots, mux capture thread: 65% publishing, of it 35% waiting on guardian
  replies and 23% in `GuardianCheckpointDescriptorV1::from_live_capture`. That runs twice per
  publication (`publish` and `bind_published`), and each run decodes the payload the parser
  just encoded twice (the artifact descriptor's `from_live_capture`, then
  `validate_canonical_payload`).

## Fix A: guardian replay rereads remember settled members (`runs/replay-*`)

Base and fixed release-perf guardians with the same release-interactive mux server (built from
fc1aa61b8's change); base, fix, fix, base. The fixed guardian is gate 5130b7bdf; the landed
change (2faddbeb8) has the same code and differs only in comments and in its new test.

| run | Replay ops | Replay time | per op | worker busy | gen 1 | gen 2 |
|---|---|---|---|---|---|---|
| base 1 | 10,268 | 81.1 s | 7.9 ms | 169.6 s | 33.7 s | 104.4 s |
| fix 1 | 12,626 | 36.5 s | 2.9 ms | 130.5 s | 38.5 s | 104.7 s |
| fix 2 | 12,640 | 33.3 s | 2.6 ms | 131.2 s | 27.5 s | 90.9 s |
| base 2 | 11,190 | 69.0 s | 6.2 ms | 165.6 s | 28.4 s | 86.8 s |

The worker's time per Replay operation (pages and opens) falls by more than half; capture times
stay within the noise. Busy time compares conservatively: fix 2's whole run includes a third
generation, which neither base run reached before its sandbox stopped.
Every run brings all 50 panes back complete.

## Fix B: a first-root capture reads at most 1 MiB of each store (`runs/cold-*`)

Release-perf mux servers without and with the cap, both with fix A's guardian; base, cap, cap,
base. The base build is gate 831aa8ebf (main's mux code; the gate changes only the guardian), the
cap build gate ce11b0178; the landed change (8954152f5) also caps a pane whose share of the
capture is under 1 MiB, and none of these 50 panes had one (each share was 64 MiB).

| run | first root after ready | first-root capture | mean first-root candidate |
|---|---|---|---|
| base 1 | 53 s | 28.5 s | 900 KiB |
| cap 1 | 46 s | 22.3 s | 900 KiB |
| cap 2 | 46 s | 20.7 s | 900 KiB |
| base 2 | 60 s | 32.3 s | 900 KiB |

The last column is the mean size of each pane's first catalog candidate, the file holding its
first-root checkpoint (`first-root-sizes.sh <sandbox>`, listed per pane in
`runs/cold-*.first-root-sizes.txt`; every one of the 200 files is 921,2xx bytes). The checkpoints
keep the same size: the store's rows take about the bytes their encoding does (127 and 135 a
row), so 1 MiB of store fills the 1 MiB checkpoint. Every restore brings all 50 panes back
complete, each store continuing into its recovered rows.

## Not changed

- `GuardianCheckpointDescriptorV1::from_live_capture` decodes the payload the parser just
  encoded up to four times per publication (23% of a full-size capture thread); it mints
  publication authority, so memoizing it is a security-boundary decision.
- `verify_logical_ledger_digest_from_store` recomputes a whole store's ledger digest on every
  snapshot (about 36 ms a pane: 1,783 of the 6,621 `materialize_cold_history` samples of the
  first-root profile, a part the cap does not shrink).
- Guardian per-chunk censuses and per-record F_FULLFSYNC pairs: changing either changes
  durability reasoning.

A run that sampled the mux during the flood recorded two scrollback durability overload gaps
(`sample` stalls the mux; the gaps are the designed overload behavior); its numbers are not in
these tables.
