#!/usr/bin/env python3
"""Run a command under a wall-clock bound, killing its descendants on expiry.

Exits with the command's own status, or 124 if the bound expired. The bound is
a failure bound surfaced to a human -- no test's correctness depends on it.
"""

import os
import subprocess
import sys

WINDOWS = os.name == "nt"


def _is_live(pid):
    """True if `pid` names a process that is not a zombie. `ps -o stat=`
    prints just the STAT column (`=` as the header suppresses it, a form
    both GNU/procps and BSD `ps` accept); a zombie's state starts with `Z`
    on both. A zombie already exited and released everything it held
    (including any pipe the watched command's descendants might have kept
    open) -- it persists only because its parent has not reaped it, which is
    not something a signal can change. `pid` having already been reaped
    entirely (no `ps` output at all) counts as not live too."""
    result = subprocess.run(
        ["ps", "-o", "stat=", "-p", str(pid)], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True
    )
    stat = result.stdout.strip()
    return bool(stat) and not stat.startswith("Z")


def kill_tree_posix(proc):
    """SIGKILL every LIVE process in the session `start_new_session=True`
    made `proc` the leader of. `killpg` alone would reach only `proc`'s own
    process group, missing a descendant that moved itself into a group of
    its own -- exactly what cosca's `.contain()` does to a contained child --
    so this goes by session (`pkill -s`) instead, both procps and BSD
    `pkill` support it. Repeated until `pgrep -s` confirms no OTHER LIVE
    member of the session is left: a single pass can race a process that
    forks between `pkill`'s snapshot and the signal actually landing, but
    the loop still terminates, because a process that has been SIGKILLed
    cannot fork another one. Liveness (not just identity) is what the loop
    waits on, because a session member other than `proc.pid` that gets
    SIGKILLed also becomes a zombie -- held open until *its own* parent
    reaps it, which this process has no way to force -- and looping on
    `pgrep -s` alone, which still lists zombies, would spin forever waiting
    for a reap that may never come. `proc.pid` itself is excluded by
    identity rather than liveness: once killed it sits as a zombie, live or
    not, until the caller's own `proc.wait()` reaps it; that reap happens
    right after this call returns, so waiting on it here would deadlock
    against itself."""
    sid = str(proc.pid)
    while True:
        subprocess.run(["pkill", "-KILL", "-s", sid], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        found = subprocess.run(["pgrep", "-s", sid], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        candidates = {int(pid) for pid in found.stdout.split()} - {proc.pid}
        if not any(_is_live(pid) for pid in candidates):
            return


def kill_tree_windows(proc):
    """`taskkill /T` walks the child's descendants; `Popen.kill`
    (`TerminateProcess` on the handle `Popen` already owns) reaches only the
    child itself, but backstops a `taskkill` that failed to kill the root,
    so the `wait()` below is never unbounded. `taskkill` ships with every
    Windows."""
    result = subprocess.run(
        ["taskkill", "/T", "/F", "/PID", str(proc.pid)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode != 0:
        stderr = result.stderr.decode(errors="replace").strip()
        print(f"run-with-timeout: taskkill failed (exit {result.returncode}): {stderr}", file=sys.stderr)
    proc.kill()


def main(argv):
    if len(argv) < 3:
        sys.exit(f"usage: {argv[0]} <seconds> <command> [args...]")
    seconds = float(argv[1])
    command = argv[2:]

    # POSIX: a new session, so `pkill -s` reaches descendants. Windows has no
    # equivalent -- CPython's `Popen` silently ignores the keyword there
    # rather than rejecting it, but it is left out anyway, since the kill on
    # that platform goes by process tree instead.
    kwargs = {} if WINDOWS else {"start_new_session": True}
    proc = subprocess.Popen(command, **kwargs)
    try:
        code = proc.wait(timeout=seconds)
    except subprocess.TimeoutExpired:
        print(f"run-with-timeout: {seconds:g}s expired: {' '.join(command)}", file=sys.stderr)
        (kill_tree_windows if WINDOWS else kill_tree_posix)(proc)
        proc.wait()
        return 124
    # A signalled POSIX child reports as a negative returncode; report it the
    # way a shell does, so `|| status=$?` sees a plausible status.
    return code if code >= 0 else 128 - code


if __name__ == "__main__":
    sys.exit(main(sys.argv))
