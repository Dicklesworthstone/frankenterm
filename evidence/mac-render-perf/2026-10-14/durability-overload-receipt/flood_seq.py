#!/usr/bin/python3 -I
"""Full-speed producer of numbered 64-byte lines, written in 64 KiB chunks."""
import sys

out = sys.stdout.buffer
n = 0
while True:
    out.write(b"".join(b"seq %012d " % i + b"y" * 43 + b"\r\n" for i in range(n, n + 1024)))
    out.flush()
    n += 1024
