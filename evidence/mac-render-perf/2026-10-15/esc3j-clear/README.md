# ft-qvt9l: the cost of ESC[3J in a pane, before and after 0a4d33e6d

Ink-based TUIs, Claude Code among them, redraw a frame taller than the
terminal with `ESC[2J ESC[3J ESC[H`. Each ESC[3J used to publish a durable
scrollback clear under the pane's terminal lock.

Probe: `scripts/measure-mux-esc3j-cost.sh <dir with frankenterm-mux-server> <3j|2j> 1000`
runs a private mux server (own HOME and socket, stopped by its own pid) with
one pane that prints 50 lines and then a clear, 1000 times. The pane can only
print as fast as the mux reads it, so the loop's wall time is the server's
cost. `3j` clears with ESC[2J ESC[3J ESC[H, `2j` with ESC[2J ESC[H only.

mac-studio (M5 Ultra, macOS 27, load 29-32), release-interactive bundles,
2026-10-15 10:39 EDT, interleaved:

| server | with ESC[3J (s) | without (s) |
|---|---|---|
| bundle-48140ff (before) | 2.130, 2.541 | 0.080, 0.079 |
| bundle-0a4d33e (after) | 0.067, 0.080 | 0.072, 0.082 |

An earlier pair on the before-server gave 1.856 / 9.256 s with ESC[3J
against 0.086 / 0.149 s without, i.e. 2-9 ms of terminal-lock time per clear.
After the change a repeated clear with nothing spilled since the last one
reuses that clear and costs nothing extra. The first clear after rows reached
the durable store is still a real, synchronous one.
