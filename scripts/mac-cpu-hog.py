#!/usr/bin/env python3
"""Run a command under a fixed synthetic CPU hog (ft-yccm0.6).

  scripts/mac-cpu-hog.py N -- COMMAND [ARGS...]

Starts N busy processes, each pinned to default QoS (the class of every
unclassified busy thread on a loaded host), runs COMMAND, then stops them and
exits with COMMAND's status. The same hog as mac-gui-throughput.py --cpu-hog,
for anything else that should run under reproducible contention, for example
a native test of the Metal render thread with and without FT_KEEP_TASK_ROLE=1:

  scripts/mac-cpu-hog.py 12 -- cargo test -p frankenterm-gui --bin frankenterm-gui \\
      frames_keep_presenting_while_the_main_thread_is_blocked
"""

import os
import signal
import subprocess
import sys

# Pin to QOS_CLASS_DEFAULT, say ready, spin without allocating.
HOG_SOURCE = ("import ctypes;ctypes.CDLL(None).pthread_set_qos_class_self_np(0x15,0);"
              "print('ready',flush=True);any(iter(int,1))")


def main(argv):
    if len(argv) < 3 or argv[1] != "--" or not argv[0].isdigit():
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        return 2
    threads, command = int(argv[0]), argv[2:]
    hogs = []
    try:
        for _ in range(threads):
            hogs.append(subprocess.Popen([sys.executable, "-I", "-c", HOG_SOURCE, "mac-cpu-hog"],
                                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True))
        for hog in hogs:
            if hog.stdout.readline().strip() != "ready":
                print(f"mac-cpu-hog: hog pid {hog.pid} did not start", file=sys.stderr)
                return 1
        print(f"mac-cpu-hog: {threads} default-QoS busy processes running: {[hog.pid for hog in hogs]}",
              file=sys.stderr, flush=True)
        return subprocess.call(command)
    finally:
        for hog in hogs:
            if hog.poll() is None:
                os.kill(hog.pid, signal.SIGTERM)
        for hog in hogs:
            hog.wait(timeout=10)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
