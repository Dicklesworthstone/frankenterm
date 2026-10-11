# T0 on a release-interactive FrankenTerm.app built from 11ec187f6

The operator's primary test, `time cat color-emoji-random.bin`, run by
`scripts/mac-gui-throughput.sh` on mac-studio on 2026-10-11 (receipt.json here).

- Build: FrankenTerm.app from 11ec187f6859ca03dcb22e09e926cd36dcf2ae30, profile
  `release-interactive`, target aarch64-apple-darwin, built natively on mac-studio
  and packaged by `scripts/create-macos-bundle.sh --skip-build` with the pinned
  browser runtime. Atomic build id
  5d62dc3891cc10d42c9f3a91a0e5daf9dd28ac0c0963ba8de004a51913f907f5; the offline
  component verifier reports ok (587 files).
- Host: Apple M5 Ultra (Mac17,15, 12P + 24E cores, 256 GiB), macOS 27.0.1, one
  60 Hz 1920x1200 display. Another project's release jobs kept the load average
  at 14-16 during the runs.
- Corpus: /Users/jemanuel/color-emoji-random.bin, 749,801,000 bytes, sha256
  a442d89593da3bbfd1d059e186b999387098c2aa7d8d4a8df88f47b811c43767.

| Arm | Runs (ABBA order) | Mean total |
|---|---|---|
| FrankenTerm | 9.417 s, 9.039 s | 9.236 s |
| Ghostty 1.3.2-main+8f0dd3709 | 9.436 s, 10.477 s | 9.960 s |

No beach-ball reading and no FPS: the harness ran over SSH, which has neither
Screen Recording nor Accessibility permission, so it ran with `--no-fps
--no-beachball-probe`. The harness also withholds a ratio verdict because
Ghostty is not pinned and its two runs differ by 7.4% (limit 5%). Treat the
ordering, not the 7% gap, as the finding: on the operator's file this build
drains at least as fast as Ghostty, where 0.15.2 beach-balled.
