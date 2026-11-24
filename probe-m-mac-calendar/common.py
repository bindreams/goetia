"""Helpers shared by the m-mac-calendar probes. Run as root, on a throwaway macOS machine only.

A job is a system LaunchDaemon with a `StartCalendarInterval` that appends the epoch to a file when it runs.
UserEventAgent logs `Registered StartCalendarInterval: <label>.<n>: <next fire>` for each dict, and the probes
read those lines with `log show`, which (measured) does not drop lines the way a live `log stream` can.
"""

from __future__ import annotations

import atexit
import os
import plistlib
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

UEA = "system/com.apple.UserEventAgent-System"
DAEMONS = Path("/Library/LaunchDaemons")
REGISTERED = re.compile(r"Registered StartCalendarInterval: (\S+)\.(\d+): (.*)$")


def say(*parts: object) -> None:
    print(*parts, flush=True)


COMMAND_BOUND = 240.0


def run(args: list[str], env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    """Runs a command; one that is still running after COMMAND_BOUND seconds is killed and reported as exit 124."""
    try:
        return subprocess.run(args, capture_output=True, text=True, env=env, check=False, timeout=COMMAND_BOUND)
    except subprocess.TimeoutExpired as expired:
        say(f"COMMAND TIMED OUT after {COMMAND_BOUND:.0f} s: {args}")
        partial = expired.stdout.decode() if isinstance(expired.stdout, bytes) else (expired.stdout or "")
        return subprocess.CompletedProcess(args, 124, partial, "timed out")


def platform_header() -> None:
    for command in (["sw_vers"], ["uname", "-a"], ["csrutil", "status"], ["date"], ["systemsetup", "-gettimezone"]):
        result = run(command)
        say(f"$ {' '.join(command)}\n{result.stdout.strip()}{result.stderr.strip()}")
    say(f"ImageOS={os.environ.get('ImageOS', '')} ImageVersion={os.environ.get('ImageVersion', '')}")
    say(f"readlink /etc/localtime: {os.readlink('/etc/localtime')}")


def utc(text: str) -> datetime:
    return datetime.fromisoformat(text).replace(tzinfo=timezone.utc)


class Clock:
    """Moves the wall clock and puts it back at the start time plus the monotonic time elapsed."""

    def __init__(self) -> None:
        self.epoch0 = int(time.time())
        self.mono0 = time.monotonic()
        self.changed = False

    @staticmethod
    def _date_u(when: datetime) -> None:
        result = run(["date", "-u", when.strftime("%m%d%H%M%Y.%S")])
        if result.returncode != 0:
            raise RuntimeError(f"date -u failed: {result.stderr}")

    def set(self, when: datetime) -> None:
        run(["systemsetup", "-setusingnetworktime", "off"])
        self.changed = True
        self._date_u(when)
        say(f"clock set: {datetime.now(timezone.utc):%Y-%m-%d %H:%M:%S} UTC")

    def restore(self) -> None:
        if self.changed:
            target = self.epoch0 + int(time.monotonic() - self.mono0)
            self._date_u(datetime.fromtimestamp(target, timezone.utc))
            self.changed = False
            say(f"clock restored: {datetime.now(timezone.utc):%Y-%m-%d %H:%M:%S} UTC")


class Jobs:
    """The calendar jobs of one probe, and the way to remove every one of them."""

    def __init__(self) -> None:
        self.work = Path(tempfile.mkdtemp(prefix="goetia-probe-cal."))
        self.labels: list[str] = []

    def add(self, label: str, interval: dict[str, int] | list[dict[str, int]]) -> None:
        plist = {
            "Label": label,
            "ProgramArguments": ["/bin/sh", "-c", 'date +%s >> "$0"', str(self.work / f"{label}.fires")],
            "StartCalendarInterval": interval,
        }
        path = DAEMONS / f"{label}.plist"
        path.write_bytes(plistlib.dumps(plist))
        shutil.chown(path, "root", "wheel")
        path.chmod(0o644)
        self.labels.append(label)

    def bootstrap(self, label: str) -> None:
        result = run(["launchctl", "bootstrap", "system", str(DAEMONS / f"{label}.plist")])
        if result.returncode != 0:
            say(f"BOOTSTRAP FAILED {label}: {result.stderr.strip()}")

    def drop(self, label: str) -> None:
        run(["launchctl", "bootout", f"system/{label}"])
        (DAEMONS / f"{label}.plist").unlink(missing_ok=True)
        if label in self.labels:
            self.labels.remove(label)

    def drop_all(self) -> None:
        for label in list(self.labels):
            self.drop(label)

    def fires(self, label: str) -> list[int]:
        path = self.work / f"{label}.fires"
        return [int(line) for line in path.read_text().split()] if path.exists() else []

    def close(self) -> None:
        self.drop_all()
        shutil.rmtree(self.work, ignore_errors=True)


def install_cleanup(jobs: Jobs, clock: Clock) -> None:
    def cleanup() -> None:
        jobs.close()
        clock.restore()

    atexit.register(cleanup)
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))


def log_show(prefix: str, window: str = "3m") -> list[str]:
    result = run(
        ["log", "show", "--last", window, "--info", "--debug", "--style", "compact", "--predicate", f'eventMessage CONTAINS "{prefix}"']
    )
    return result.stdout.splitlines()


def registered_lines(prefix: str, labels: list[str], bound_seconds: float = 120.0) -> dict[str, list[str]]:
    """Each label's `Registered` dates, polled with `log show` until every label has one.

    The bound is the failure bound: a label with no line by then is reported as missing, not as proof of anything.
    """
    found: dict[str, list[str]] = {}
    start = time.monotonic()
    while True:
        found = {}
        for line in log_show(prefix):
            match = REGISTERED.search(line)
            if match and match.group(1) in labels:
                found.setdefault(match.group(1), []).append(match.group(3))
        if all(label in found for label in labels):
            return found
        if time.monotonic() - start > bound_seconds:
            say(f"NO REGISTERED LINE in {bound_seconds:.0f} s for: {[l for l in labels if l not in found]}")
            return found


def wait_fires(jobs: Jobs, label: str, count: int, bound_seconds: float) -> bool:
    """Waits for `count` fires of `label`; the bound is the failure bound for a fire that may never come."""
    start = time.monotonic()
    while len(jobs.fires(label)) < count:
        if time.monotonic() - start > bound_seconds:
            say(f"NO FIRE: {label} did not reach {count} fire(s) in {bound_seconds:.0f} s")
            return False
        time.sleep(0.5)
    return True


def uea_pid() -> str:
    for line in run(["launchctl", "print", UEA]).stdout.splitlines():
        parts = line.split()
        if len(parts) >= 3 and parts[0] == "pid" and parts[1] == "=":
            return parts[2]
    return ""


def restart_uea() -> None:
    old = uea_pid()
    result = run(["launchctl", "kickstart", "-k", UEA])
    say(f"kickstart -k {UEA}: exit {result.returncode} {result.stderr.strip()}")
    while True:
        new = uea_pid()
        if new and new != old:
            say(f"UserEventAgent pid {old} -> {new}")
            return


def register_canary(jobs: Jobs, label: str) -> str:
    """Registers a {Hour 3, Minute 0} job and returns its Registered date, then drops it."""
    jobs.add(label, {"Hour": 3, "Minute": 0})
    jobs.bootstrap(label)
    lines = registered_lines(label, [label]).get(label, [""])
    jobs.drop(label)
    return lines[0]


def use_zone(jobs: Jobs, zone: str) -> None:
    """Puts the machine and UserEventAgent in `zone` (measured: UserEventAgent computes in it after a restart)."""
    result = run(["systemsetup", "-settimezone", zone])
    say(f"systemsetup -settimezone {zone}: exit {result.returncode} {result.stdout.strip()}{result.stderr.strip()}")
    current = run(["systemsetup", "-gettimezone"]).stdout
    if zone not in current:
        say(f"systemsetup did not set {zone} ({current.strip()}); relinking /etc/localtime")
        say(run(["ls", "-l", "/etc/localtime"]).stdout.strip() or "/etc/localtime does not exist")
        Path("/etc/localtime").unlink(missing_ok=True)
        os.symlink(f"/var/db/timezone/zoneinfo/{zone}", "/etc/localtime")
    say(f"zone now: {run(['systemsetup', '-gettimezone']).stdout.strip()}; localtime -> {os.readlink('/etc/localtime')}")
    restart_uea()
    line = register_canary(jobs, "goetia.probe.canary")
    say(f"CANARY: {line}")
    # The time and "AM" are separated by U+202F, not a space.
    if "at 3:00:00" not in line or "AM" not in line:
        say(f"zone NOT applied: UserEventAgent does not compute in {zone}")
        sys.exit(3)
    say(f"zone ok: UserEventAgent computes in {zone}")
