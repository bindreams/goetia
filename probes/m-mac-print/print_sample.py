#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.9"
# dependencies = []
# ///
"""Questions 1 to 3: what `launchctl print system/<label>` shows around launches that were never held.

Claim measured: `print` never shows `state = running` (or xpcproxy / trampoline) with no `pid` line, and never shows
`started suspended = 1` for a launch no `debug --start-suspended` armed. Run as root on a throwaway macOS machine.

    print_sample.py <config> <amount>

configs (amount = launches per driver, or seconds for the timed ones):
  kick-true    /usr/bin/true, drivers `kickstart` it back to back, throttle 0
  kick-wrap    /bin/sh -c 'exec /usr/bin/true' (a longer xpcproxy stage), the same
  kick-sleepy  /bin/sleep 0.05 (a visible `running` stage), the same
  kick-k       /bin/sleep 600 restarted by `kickstart -k` (a kill and a start in one call)
  keepalive    KeepAlive true, /usr/bin/true, throttle 0, for <amount> seconds
  interval     12 jobs with StartInterval 1 (scheduled fires), for <amount> seconds
  bootstrap    RunAtLoad /bin/sleep 0.05, booted out and in <amount> times
  held-control `debug --start-suspended` then `kickstart -p`, <amount> times: the sampler must see `started suspended = 1`
Sampler threads read `print` as fast as it answers (one process per read) while the drivers run; each driver also reads
`print` at once after its own call. The output is a tally of (state, pid line, started suspended) per config.
"""

import collections
import os
import random
import re
import subprocess
import sys
import threading

LABEL = "goetia.probe.printsample"
SAMPLERS = 3
STATE = re.compile(r"^\tstate = (.*)$", re.M)
PID = re.compile(r"^\tpid = (\d+)$", re.M)
SUSP = re.compile(r"^\tstarted suspended = (\d+)$", re.M)
RUNS = re.compile(r"^\truns = (\d+)$", re.M)

lock = threading.Lock()
tally = collections.Counter()
samples = collections.defaultdict(list)  # class -> first few full outputs
config_name = ""
held_config = False


def lc(*args):
    p = subprocess.run(["/bin/launchctl", *args], capture_output=True, text=True)
    return p.returncode, p.stdout, p.stderr


def plist_path(label):
    return f"/Library/LaunchDaemons/{label}.plist"


def write_plist(label, args, extra=""):
    items = "".join(f"<string>{a}</string>" for a in args)
    body = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
        f'<plist version="1.0"><dict><key>Label</key><string>{label}</string>'
        f"<key>ProgramArguments</key><array>{items}</array>"
        f"<key>UserName</key><string>root</string>{extra}</dict></plist>\n"
    )
    with open(plist_path(label), "w") as f:
        f.write(body)
    os.chown(plist_path(label), 0, 0)
    os.chmod(plist_path(label), 0o644)


def bootstrap(label):
    while True:  # a bootout still tearing down answers non-zero; retry until it takes
        code, _, _ = lc("bootstrap", "system", plist_path(label))
        if code == 0:
            return
        tally["bootstrap retry"] += 1


def remove(label):
    lc("kill", "SIGKILL", f"system/{label}")
    lc("bootout", f"system/{label}")
    try:
        os.unlink(plist_path(label))
    except FileNotFoundError:
        pass


def classify(code, out):
    if code != 0:
        return f"print exit {code}", None
    s = STATE.search(out)
    p = PID.search(out)
    u = SUSP.search(out)
    state = s.group(1) if s else "(no state line)"
    key = f"state [{state}], pid line {'yes' if p else 'NO'}, started suspended {u.group(1) if u else '(none)'}"
    anomalous = False
    if state in ("running", "xpcproxy", "trampoline") and not p:
        anomalous = True
        key = "ANOMALY Q1/Q2: " + key
    if u and u.group(1) == "1" and not held_config:
        anomalous = True
        key = "ANOMALY Q3: " + key
    return key, out if anomalous else None


def record(how, label, code, out):
    key, full = classify(code, out)
    with lock:
        tally[(how, key)] += 1
        tally[("all", "samples")] += 1
        if full is not None and len(samples[key]) < 5:
            samples[key].append(f"[{how} {label}]\n{full}")


def sample(how, label):
    code, out, _ = lc("print", f"system/{label}")
    record(how, label, code, out)


def sampler(labels, stop):
    while not stop.is_set():
        sample("sampler", random.choice(labels))


def run_with_samplers(labels, drivers):
    stop = threading.Event()
    threads = [threading.Thread(target=sampler, args=(labels, stop)) for _ in range(SAMPLERS)]
    for t in threads:
        t.start()
    ds = [threading.Thread(target=d) for d in drivers]
    for t in ds:
        t.start()
    for t in ds:
        t.join()
    stop.set()
    for t in threads:
        t.join()


def runs_of(label):
    code, out, _ = lc("print", f"system/{label}")
    m = RUNS.search(out) if code == 0 else None
    return int(m.group(1)) if m else -1


def kick_driver(label, n, flags):
    def go():
        for _ in range(n):
            lc("kickstart", *flags, f"system/{label}")
            sample("driver, right after kickstart", label)

    return go


def cfg_kick(args, amount, flags=()):
    labels = [f"{LABEL}.{i}" for i in range(2)]
    for l in labels:
        write_plist(l, args, "<key>ThrottleInterval</key><integer>0</integer>")
        bootstrap(l)
    run_with_samplers(labels, [kick_driver(l, amount, flags) for l in labels])
    for l in labels:
        tally[("launches", f"runs counter of {l} at the end")] = runs_of(l)
        remove(l)


def cfg_keepalive(amount):
    write_plist(LABEL, ["/usr/bin/true"], "<key>KeepAlive</key><true/><key>ThrottleInterval</key><integer>0</integer>")
    bootstrap(LABEL)
    done = threading.Event()
    run_with_samplers([LABEL], [lambda: done.wait(amount)])  # the duration is the experiment's variable
    tally[("launches", "runs counter at the end")] = runs_of(LABEL)
    remove(LABEL)


def cfg_interval(amount):
    labels = [f"{LABEL}.{i}" for i in range(12)]
    for l in labels:
        write_plist(l, ["/bin/sh", "-c", "exec /usr/bin/true"], "<key>StartInterval</key><integer>1</integer><key>ThrottleInterval</key><integer>0</integer>")
        bootstrap(l)
    done = threading.Event()
    run_with_samplers(labels, [lambda: done.wait(amount)])
    for l in labels:
        tally[("launches", f"runs counter of {l} at the end")] = runs_of(l)
        remove(l)


def cfg_bootstrap(amount):
    write_plist(LABEL, ["/bin/sleep", "0.05"], "<key>RunAtLoad</key><true/>")

    def go():
        for _ in range(amount):
            bootstrap(LABEL)
            sample("driver, right after bootstrap", LABEL)
            lc("bootout", f"system/{LABEL}")
            sample("driver, right after bootout", LABEL)

    run_with_samplers([LABEL], [go])
    remove(LABEL)


def cfg_held(amount):
    write_plist(LABEL, ["/usr/bin/true"], "<key>ThrottleInterval</key><integer>0</integer>")
    bootstrap(LABEL)

    def go():
        for _ in range(amount):
            lc("debug", f"system/{LABEL}", "--start-suspended")
            _, out, _ = lc("kickstart", "-p", f"system/{LABEL}")
            sample("driver, right after held kickstart", LABEL)
            lc("kill", "SIGCONT", f"system/{LABEL}")
            lc("kill", "SIGKILL", f"system/{LABEL}")

    run_with_samplers([LABEL], [go])
    remove(LABEL)


def main():
    global config_name, held_config
    if os.geteuid() != 0 or len(sys.argv) != 3:
        sys.exit("run as root: print_sample.py <config> <amount>")
    config_name, amount = sys.argv[1], int(sys.argv[2])
    held_config = config_name == "held-control"
    print(f"config {config_name}, amount {amount}, samplers {SAMPLERS}", flush=True)
    try:
        if config_name == "kick-true":
            cfg_kick(["/usr/bin/true"], amount)
        elif config_name == "kick-wrap":
            cfg_kick(["/bin/sh", "-c", "exec /usr/bin/true"], amount)
        elif config_name == "kick-sleepy":
            cfg_kick(["/bin/sleep", "0.05"], amount)
        elif config_name == "kick-k":
            cfg_kick(["/bin/sleep", "600"], amount, ("-k",))
        elif config_name == "keepalive":
            cfg_keepalive(amount)
        elif config_name == "interval":
            cfg_interval(amount)
        elif config_name == "bootstrap":
            cfg_bootstrap(amount)
        elif config_name == "held-control":
            cfg_held(amount)
        else:
            sys.exit(f"unknown config {config_name}")
    finally:
        print(f"===== {config_name} =====")
        for (how, key), n in sorted(tally.items(), key=lambda kv: str(kv[0])):
            print(f"{n:9d}  {how}: {key}")
        for key, outs in samples.items():
            print(f"----- first full prints of: {key}")
            for o in outs:
                print(o)


main()
