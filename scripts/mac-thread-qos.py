#!/usr/bin/env python3
"""Per-thread scheduling probe for a macOS process (ft-yccm0.6).

Lists every thread of a process from outside it: its name, base priority,
the QoS class that base priority belongs to, current priority, run state and
CPU time. It reads libproc (proc_pidinfo PROC_PIDLISTTHREADS and
PROC_PIDTHREADINFO), which needs no privileges for the caller's own
processes and never signals or suspends the target.

macOS gives each QoS class a base priority: user-interactive 46 (47 for a
foreground app's main thread), user-initiated 37, default 31, utility 20,
background 4. A thread that never
set a class also runs at 31, so "default" covers both. Under CPU contention
the scheduler runs the higher base priorities first; a thread at 31 competes
with every unclassified busy thread on the host.

A process without an application task role, which is anything not launched
as an app (from a shell, a harness, tmux), has user-interactive and
user-initiated requests squashed to default: the request succeeds and reads
back, but the thread runs at 31. Setting the task role to
TASK_DEFAULT_APPLICATION lifts that. --self-test shows both.

  scripts/mac-thread-qos.py PID                     one snapshot
  scripts/mac-thread-qos.py PID --every 1 --for 30  CPU per thread over 30 s,
                                                    with every class it showed
  scripts/mac-thread-qos.py PID --json              machine-readable
  scripts/mac-thread-qos.py --self-test             the probe against child
                                                    processes of known classes

The harness (scripts/mac-gui-throughput.py) loads this file to sample the
measured terminal's threads during every run.
"""

import argparse
import ctypes
import ctypes.util
import json
import sys
import threading
import time

PROC_PIDLISTTHREADS = 6
PROC_PIDTHREADINFO = 5
MAXTHREADNAMESIZE = 64

# Base priority -> QoS class (osfmk/kern/thread_policy.c, thread_qos_policy_params).
# 47 is the foreground application's main thread (BASEPRI_FOREGROUND), above
# user-interactive's 46.
QOS_BY_BASE_PRIORITY = {
    47: "user-interactive (app main thread)",
    46: "user-interactive",
    37: "user-initiated",
    31: "default",
    20: "utility",
    4: "background",
}

RUN_STATES = {1: "running", 2: "stopped", 3: "waiting", 4: "uninterruptible", 5: "halted"}


class ProcThreadInfo(ctypes.Structure):
    """struct proc_threadinfo (sys/proc_info.h). The times are nanoseconds."""

    _fields_ = [
        ("pth_user_time", ctypes.c_uint64),
        ("pth_system_time", ctypes.c_uint64),
        ("pth_cpu_usage", ctypes.c_int32),
        ("pth_policy", ctypes.c_int32),
        ("pth_run_state", ctypes.c_int32),
        ("pth_flags", ctypes.c_int32),
        ("pth_sleep_time", ctypes.c_int32),
        ("pth_curpri", ctypes.c_int32),
        ("pth_priority", ctypes.c_int32),
        ("pth_maxpriority", ctypes.c_int32),
        ("pth_name", ctypes.c_char * MAXTHREADNAMESIZE),
    ]


_LIBPROC = None


def _libproc():
    global _LIBPROC
    if _LIBPROC is None:
        path = ctypes.util.find_library("proc") or "/usr/lib/libproc.dylib"
        library = ctypes.CDLL(path, use_errno=True)
        library.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p,
                                         ctypes.c_int]
        library.proc_pidinfo.restype = ctypes.c_int
        _LIBPROC = library
    return _LIBPROC


def qos_class(base_priority):
    """The QoS class a base priority belongs to, or "priority N"."""
    return QOS_BY_BASE_PRIORITY.get(base_priority, f"priority {base_priority}")


def thread_handles(pid):
    """The pid's thread handles (PROC_PIDLISTTHREADS); empty if it is gone."""
    library = _libproc()
    capacity = 256
    while True:
        buffer = (ctypes.c_uint64 * capacity)()
        size = library.proc_pidinfo(pid, PROC_PIDLISTTHREADS, 0, buffer, ctypes.sizeof(buffer))
        if size <= 0:
            return []
        count = size // ctypes.sizeof(ctypes.c_uint64)
        if count < capacity:
            return list(buffer[:count])
        capacity *= 2


def threads(pid):
    """One snapshot of every thread of `pid`: a list of dicts, in list order."""
    library = _libproc()
    out = []
    for handle in thread_handles(pid):
        info = ProcThreadInfo()
        size = library.proc_pidinfo(pid, PROC_PIDTHREADINFO, handle, ctypes.byref(info), ctypes.sizeof(info))
        if size != ctypes.sizeof(info):
            continue  # the thread exited between the two calls
        out.append({
            "handle": handle,
            "name": info.pth_name.decode("utf-8", "replace") or "(unnamed)",
            "base_priority": info.pth_priority,
            "qos": qos_class(info.pth_priority),
            "current_priority": info.pth_curpri,
            "max_priority": info.pth_maxpriority,
            "run_state": RUN_STATES.get(info.pth_run_state, str(info.pth_run_state)),
            "cpu_ns": info.pth_user_time + info.pth_system_time,
        })
    return out


class ThreadSampler(threading.Thread):
    """Samples a pid's threads every `interval` seconds until stopped, and
    summarizes each thread: its name, every QoS class and current priority it
    showed, and the CPU it used between its first and last sample."""

    def __init__(self, pid, interval=1.0):
        super().__init__(daemon=True)
        self.pid, self.interval = pid, interval
        self.stop_event = threading.Event()
        self.samples = 0
        self.per_thread = {}
        self.error = None

    def sample(self):
        now = time.monotonic()
        for thread in threads(self.pid):
            entry = self.per_thread.setdefault(thread["handle"], {
                "name": thread["name"], "first_cpu_ns": thread["cpu_ns"], "first_seen": now,
                "classes": {}, "current_priorities": {}, "running_samples": 0, "samples": 0})
            entry["name"] = thread["name"]  # a thread may name itself after it starts
            entry["last_cpu_ns"] = thread["cpu_ns"]
            entry["last_seen"] = now
            entry["samples"] += 1
            entry["classes"][thread["qos"]] = entry["classes"].get(thread["qos"], 0) + 1
            current = str(thread["current_priority"])
            entry["current_priorities"][current] = entry["current_priorities"].get(current, 0) + 1
            if thread["run_state"] == "running":
                entry["running_samples"] += 1
        self.samples += 1

    def run(self):
        try:
            while not self.stop_event.is_set():
                self.sample()
                self.stop_event.wait(self.interval)
            self.sample()
        except Exception as error:  # noqa: BLE001 -- reported in the summary
            self.error = f"{type(error).__name__}: {error}"

    def stop(self):
        self.stop_event.set()
        self.join(timeout=10)

    def summary(self):
        """Threads by CPU used while sampled, most first."""
        rows = []
        for handle, entry in self.per_thread.items():
            rows.append({
                "handle": handle,
                "name": entry["name"],
                "cpu_s": round((entry["last_cpu_ns"] - entry["first_cpu_ns"]) / 1e9, 3),
                "qos": max(entry["classes"], key=entry["classes"].get),
                "classes": entry["classes"],
                "current_priorities": entry["current_priorities"],
                "running_samples": entry["running_samples"],
                "samples": entry["samples"],
            })
        rows.sort(key=lambda row: -row["cpu_s"])
        return {"pid": self.pid, "interval_s": self.interval, "samples": self.samples, "error": self.error,
                "threads": rows}


# A child with one named thread per QoS class. With "role" it first sets its
# task role to TASK_DEFAULT_APPLICATION (task_policy_set, TASK_CATEGORY_POLICY).
SELF_TEST_CHILD = r"""
import ctypes, sys, threading
libc = ctypes.CDLL(None)
if sys.argv[1] == "role":
    task = ctypes.c_uint.in_dll(libc, "mach_task_self_").value
    role = ctypes.c_int(7)
    assert libc.task_policy_set(task, 1, ctypes.byref(role), 1) == 0
started = threading.Barrier(6)
def worker(name, qos):
    libc.pthread_setname_np(name.encode())
    assert libc.pthread_set_qos_class_self_np(qos, 0) == 0
    started.wait()
    sys.stdin.read()
for name, qos in (("ui", 0x21), ("uinit", 0x19), ("default", 0x15), ("utility", 0x11), ("background", 0x09)):
    threading.Thread(target=worker, args=("selftest-" + name, qos), daemon=True).start()
started.wait()
print("ready", flush=True)
sys.stdin.read()
"""


def self_test():
    import subprocess

    def priorities(mode):
        child = subprocess.Popen([sys.executable, "-I", "-c", SELF_TEST_CHILD, mode], stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, text=True)
        try:
            assert child.stdout.readline().strip() == "ready", f"{mode} child did not start"
            return {thread["name"].removeprefix("selftest-"): thread["base_priority"]
                    for thread in threads(child.pid) if thread["name"].startswith("selftest-")}
        finally:
            child.stdin.close()
            child.wait(timeout=10)

    plain, role = priorities("plain"), priorities("role")
    assert role == {"ui": 46, "uinit": 37, "default": 31, "utility": 20, "background": 4}, role
    assert {name: plain[name] for name in ("default", "utility", "background")} == {
        "default": 31, "utility": 20, "background": 4}, plain
    assert {qos_class(role[name]) for name in role} == {"user-interactive", "user-initiated", "default",
                                                        "utility", "background"}
    print(f"self-test: ok. Base priorities without an application role {plain}; with "
          f"TASK_DEFAULT_APPLICATION {role}")


def format_rows(rows, cpu_key):
    lines = [f"{'thread':<36} {'qos (base pri)':<26} {'cur pri':<10} {cpu_key:>9}"]
    for row in rows:
        lines.append(f"{row['name'][:36]:<36} {row['qos_text']:<26} {row['cur_text']:<10} {row[cpu_key]:>9}")
    return "\n".join(lines)


def main(argv):
    parser = argparse.ArgumentParser(prog="mac-thread-qos.py", description=__doc__.split("\n\n")[0])
    parser.add_argument("pid", type=int, nargs="?")
    parser.add_argument("--every", type=float, help="sample every N seconds (with --for)")
    parser.add_argument("--for", dest="duration", type=float, help="sample for N seconds")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if sys.platform != "darwin":
        print("mac-thread-qos: macOS only", file=sys.stderr)
        return 2
    if args.self_test:
        self_test()
        return 0
    if args.pid is None:
        parser.error("a PID is required")
    if args.duration:
        sampler = ThreadSampler(args.pid, args.every or 1.0)
        sampler.start()
        time.sleep(args.duration)
        sampler.stop()
        report = sampler.summary()
        if args.json:
            print(json.dumps(report, indent=2))
            return 0
        rows = [dict(row, qos_text=", ".join(f"{name} x{count}" for name, count in row["classes"].items()),
                     cur_text=",".join(row["current_priorities"]), cpu=f"{row['cpu_s']:.3f}")
                for row in report["threads"]]
        print(f"pid {args.pid}: {report['samples']} samples every {args.every or 1.0} s"
              + (f"; error {report['error']}" if report["error"] else ""))
        print(format_rows(rows, "cpu"))
        return 0
    snapshot = threads(args.pid)
    if not snapshot:
        print(f"mac-thread-qos: no threads for pid {args.pid} (gone, or not readable)", file=sys.stderr)
        return 1
    if args.json:
        print(json.dumps(snapshot, indent=2))
        return 0
    rows = [dict(thread, qos_text=f"{thread['qos']} ({thread['base_priority']})",
                 cur_text=str(thread["current_priority"]), cpu=f"{thread['cpu_ns'] / 1e9:.3f}")
            for thread in snapshot]
    print(f"pid {args.pid}: {len(rows)} threads")
    print(format_rows(rows, "cpu"))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
