#!/usr/bin/python3 -I
"""Rate-limited line producer: LINES_PER_TICK 80-column lines every 10 ms."""
import sys
import time

per_tick = int(sys.argv[1])
line = b"agent output line %09d " + b"x" * 50 + b"\r\n"
n = 0
next_tick = time.monotonic()
out = sys.stdout.buffer
while True:
    out.write(b"".join(line % (n + i) for i in range(per_tick)))
    out.flush()
    n += per_tick
    next_tick += 0.01
    delay = next_tick - time.monotonic()
    if delay > 0:
        time.sleep(delay)
