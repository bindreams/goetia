#!/usr/bin/env python3
"""launchd ExitTimeOut probe for goetia #263 (throwaway). Run as root on a macOS runner.

For each ExitTimeOut setting (unset, 5, 0, 120) and each stop method (`launchctl bootout`,
`launchctl kill SIGTERM`), loads a LaunchDaemon (UserName root) running sigterm-logger, which logs the
time SIGTERM arrives and never exits by itself. Prints: when the stop command returned, when SIGTERM
arrived, and when the process was gone (that is launchd's SIGKILL), all in seconds from the command.
All jobs run concurrently; an observation ends when the process is gone or after BOUND seconds.
"""
import os
import plistlib
import subprocess
import sys
import threading
import time

BOUND = 130.0  # failure bound for "wait forever" cases (surfaced to a human), not a synchronisation
BIN = sys.argv[1]
WORK = "/tmp/gs"
CONFIGS = [("unset", None), ("1", 1), ("0", 0)]
METHODS = ["bootout"]
results = {}


def run(*cmd):
    p = subprocess.run(cmd, capture_output=True, text=True)
    return p.returncode, (p.stdout + p.stderr).strip()


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def log_lines(path):
    try:
        return open(path).read().splitlines()
    except FileNotFoundError:
        return []


def observe(method, name, timeout):
    label = f"com.probe.gs.{method}.{name}"
    log = f"{WORK}/{label}.log"
    plist = f"/Library/LaunchDaemons/{label}.plist"
    d = {"Label": label, "ProgramArguments": [BIN, log], "UserName": "root", "RunAtLoad": True}
    if timeout is not None:
        d["ExitTimeOut"] = timeout
    with open(plist, "wb") as f:
        plistlib.dump(d, f)
    os.chmod(plist, 0o644)
    r = {"label": label}
    results[label] = r
    rc, out = run("launchctl", "bootstrap", "system", plist)
    r["bootstrap"] = f"rc={rc} {out}"
    while not any(l.startswith("started") for l in log_lines(log)):
        time.sleep(0.05)
    pid = int([l for l in log_lines(log) if l.startswith("started")][0].split("pid=")[1].split()[0])
    r["pid"] = pid
    barrier.wait()  # start all stops together
    t0 = time.time()
    if method == "bootout":
        rc, out = run("launchctl", "bootout", f"system/{label}")
    else:
        rc, out = run("launchctl", "kill", "SIGTERM", f"system/{label}")
    r["cmd_returned_after"] = round(time.time() - t0, 3)
    r["cmd_rc"] = f"{rc} {out}"
    gone = None
    while time.time() - t0 < BOUND:
        if not alive(pid):
            gone = round(time.time() - t0, 3)
            break
        time.sleep(0.02)
    r["gone_after"] = gone  # None: still alive at BOUND
    terms = [float(l.split("t=")[1]) - t0 for l in log_lines(log) if l.startswith("SIGTERM")]
    r["sigterm_arrived_after"] = [round(t, 3) for t in terms]
    # cleanup: only what this probe created
    if alive(pid):
        os.kill(pid, 9)
    run("launchctl", "bootout", f"system/{label}")
    os.remove(plist)


os.makedirs(WORK, exist_ok=True)
jobs = [(m, n, t) for m in METHODS for (n, t) in CONFIGS]
barrier = threading.Barrier(len(jobs))
threads = [threading.Thread(target=observe, args=j) for j in jobs]
for t in threads:
    t.start()
for t in threads:
    t.join()
for m in METHODS:
    for n, _ in CONFIGS:
        r = results[f"com.probe.gs.{m}.{n}"]
        print(f"{m:8s} ExitTimeOut={n:6s} bootstrap[{r['bootstrap']}] cmd_returned_after={r['cmd_returned_after']}s "
              f"cmd_rc[{r['cmd_rc']}] SIGTERM_after={r['sigterm_arrived_after']} "
              f"process_gone_after={r['gone_after']} (None = still alive at {BOUND}s)")
