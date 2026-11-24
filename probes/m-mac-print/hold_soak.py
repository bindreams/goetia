#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.9"
# dependencies = []
# ///
"""Question 4: can the system end a launch that launchd started suspended (`launchctl debug --start-suspended`)?

    hold_soak.py <soak-seconds> <pressure-rounds>

Holds one launch in each of JOBS jobs (three plist variants), leaves them stopped for <soak-seconds> reading each every
30 s, then runs memory pressure for <pressure-rounds> rounds (`memory_pressure -l critical`, then a Python allocator
that touches pages until it is killed or reaches 90 % of RAM), reading the held launches each second and the system's
pressure level. At the end it resumes every launch and checks it exits. Run as root on a throwaway macOS machine.
The soak length is the experiment's variable, not a wait for a state.
"""

import os
import re
import subprocess
import sys
import threading
import time

JOBS = 24
PREFIX = "goetia.probe.holdsoak"
VARIANTS = {
    "default": "",
    "background": "<key>ProcessType</key><string>Background</string>",
    "pressured-exit": "<key>ProcessType</key><string>Adaptive</string><key>EnablePressuredExit</key><true/><key>EnableTransactions</key><false/>",
}
PID = re.compile(r"^\tpid = (\d+)$", re.M)
STATE = re.compile(r"^\tstate = (.*)$", re.M)
SUSP = re.compile(r"^\tstarted suspended = (\d+)$", re.M)


def run(*args):
    p = subprocess.run(list(args), capture_output=True, text=True)
    return p.returncode, p.stdout, p.stderr


def lc(*args):
    return run("/bin/launchctl", *args)


def write_plist(label, extra):
    body = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
        f'<plist version="1.0"><dict><key>Label</key><string>{label}</string>'
        "<key>ProgramArguments</key><array><string>/usr/bin/true</string></array>"
        f"<key>UserName</key><string>root</string>{extra}</dict></plist>\n"
    )
    path = f"/Library/LaunchDaemons/{label}.plist"
    with open(path, "w") as f:
        f.write(body)
    os.chown(path, 0, 0)
    os.chmod(path, 0o644)


def ps_stat(pid):
    code, out, _ = run("/bin/ps", "-o", "stat=", "-p", str(pid))
    return out.strip() if code == 0 and out.strip() else "(gone)"


def pressure_level():
    return run("/usr/sbin/sysctl", "-n", "kern.memorystatus_vm_pressure_level")[1].strip()


class Job:
    def __init__(self, i, variant):
        self.label = f"{PREFIX}.{i}"
        self.variant = variant
        self.pid = -1
        self.first_bad = None  # (phase, what) of the first read that was not "stopped"

    def start_held(self):
        write_plist(self.label, VARIANTS[self.variant])
        while lc("bootstrap", "system", f"/Library/LaunchDaemons/{self.label}.plist")[0] != 0:
            pass
        code, _, err = lc("debug", f"system/{self.label}", "--start-suspended")
        if code:
            sys.exit(f"debug failed for {self.label}: {code} {err}")
        _, out, _ = lc("kickstart", "-p", f"system/{self.label}")
        self.pid = int(out.strip())
        # Wait for the held state itself, as goetia's read does: the process reads stopped once xpcproxy has exec'd.
        # 60 s is this probe's failure bound, printed when it is hit.
        deadline = time.monotonic() + 60
        while "T" not in ps_stat(self.pid):
            if time.monotonic() > deadline:
                sys.exit(f"{self.label} pid {self.pid} never read stopped: {ps_stat(self.pid)}")

    def read(self, phase):
        st = ps_stat(self.pid)
        code, out, _ = lc("print", f"system/{self.label}")
        s, p, u = STATE.search(out), PID.search(out), SUSP.search(out)
        desc = f"ps [{st}], print exit {code}, state [{s.group(1) if s else '-'}], pid {p.group(1) if p else '-'}, suspended {u.group(1) if u else '-'}"
        ok = "T" in st and code == 0 and p and int(p.group(1)) == self.pid and u and u.group(1) == "1"
        if not ok and self.first_bad is None:
            self.first_bad = (phase, desc)
        return ok, desc


def read_all(jobs, phase, hist):
    for j in jobs:
        ok, desc = j.read(phase)
        hist[(phase, ok)] = hist.get((phase, ok), 0) + 1


def pressure_rounds(jobs, rounds, hist):
    memsize = int(run("/usr/sbin/sysctl", "-n", "hw.memsize")[1])
    levels = {}
    for r in range(rounds):
        for name, cmd in (
            ("memory_pressure -l critical", ["/usr/bin/memory_pressure", "-l", "critical", "-s", "60"]),
            ("python allocator to 90 % of RAM", [sys.executable, "-I", "-c", ALLOC, str(int(memsize * 0.9))]),
        ):
            print(f"pressure round {r + 1}: {name}", flush=True)
            proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            while proc.poll() is None:  # the load's own exit ends the round
                lvl = pressure_level()
                levels[lvl] = levels.get(lvl, 0) + 1
                read_all(jobs, f"pressure: {name}", hist)
            out = proc.stdout.read()
            print(f"  exit {proc.returncode}; output tail: {out[-300:]!r}", flush=True)
    print(f"kern.memorystatus_vm_pressure_level seen over the rounds (1 normal, 2 warn, 4 critical): {levels}", flush=True)


ALLOC = """
import sys, time
want = int(sys.argv[1]); chunks = []; got = 0
try:
    while got < want:
        b = bytearray(64 << 20)
        for i in range(0, len(b), 4096):
            b[i] = 1
        chunks.append(b); got += len(b)
    t = time.monotonic()
    while time.monotonic() - t < 45:
        for b in chunks[::8]:
            b[0] = 2
finally:
    print('allocated', got >> 20, 'MiB')
"""


def main():
    if os.geteuid() != 0 or len(sys.argv) != 3:
        sys.exit("run as root: hold_soak.py <soak-seconds> <pressure-rounds>")
    soak, rounds = int(sys.argv[1]), int(sys.argv[2])
    names = list(VARIANTS)
    jobs = [Job(i, names[i % len(names)]) for i in range(JOBS)]
    hist = {}
    try:
        for j in jobs:
            j.start_held()
        print(f"{JOBS} launches held at {time.strftime('%X')}", flush=True)
        start = time.monotonic()
        while time.monotonic() - start < soak:
            read_all(jobs, "soak", hist)
            time.sleep(30)
        print(f"soak of {soak} s done; every job read held in 'soak': {[k for k in hist if k[0]=='soak']}", flush=True)
        pressure_rounds(jobs, rounds, hist)
        read_all(jobs, "after pressure", hist)
        # Release each: SIGCONT must let the program run and exit.
        released = 0
        for j in jobs:
            lc("kill", "SIGCONT", f"system/{j.label}")
            deadline = time.monotonic() + 30
            while ps_stat(j.pid) != "(gone)" and time.monotonic() < deadline:
                pass
            if ps_stat(j.pid) == "(gone)":
                released += 1
        print(f"released by SIGCONT and exited: {released} of {JOBS}", flush=True)
    finally:
        print("===== reads: (phase, every read was 'stopped, print names the pid, started suspended 1') -> count")
        for k, n in sorted(hist.items(), key=str):
            print(f"{n:9d}  {k}")
        for v in names:
            bad = [j for j in jobs if j.variant == v and j.first_bad]
            print(f"variant {v}: {len([j for j in jobs if j.variant == v])} jobs, {len(bad)} with a read that was not held")
            for j in bad:
                print(f"  {j.label} pid {j.pid}: first bad read in {j.first_bad[0]}: {j.first_bad[1]}")
        for j in jobs:
            lc("kill", "SIGKILL", f"system/{j.label}")
            lc("bootout", f"system/{j.label}")
            try:
                os.unlink(f"/Library/LaunchDaemons/{j.label}.plist")
            except FileNotFoundError:
                pass


main()
