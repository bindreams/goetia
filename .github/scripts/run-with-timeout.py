#!/usr/bin/env python3
"""Run a command under a wall-clock bound, killing its descendants on expiry.

Exits with the command's own status, or 124 if the bound expired. The bound is
a failure bound surfaced to a human -- no test's correctness depends on it.
"""

import os
import subprocess
import sys

WINDOWS = os.name == "nt"


def kill_tree_posix(proc):
    """SIGKILL the session `start_new_session=True` put the child in, so a
    descendant the command left behind dies with it."""
    os.killpg(os.getpgid(proc.pid), 9)


def kill_tree_windows(proc):
    """`taskkill /T` walks the child's descendants; `Popen.kill` reaches only
    the child itself. `taskkill` ships with every Windows."""
    subprocess.run(
        ["taskkill", "/T", "/F", "/PID", str(proc.pid)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def main(argv):
    if len(argv) < 3:
        sys.exit(f"usage: {argv[0]} <seconds> <command> [args...]")
    seconds = float(argv[1])
    command = argv[2:]

    # POSIX: a new session, so `killpg` reaches descendants. Windows has no
    # equivalent and rejects the keyword, so the kill goes by process tree.
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
