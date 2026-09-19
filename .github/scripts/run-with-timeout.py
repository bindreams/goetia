#!/usr/bin/env python3
"""Run a command under a wall-clock bound. On expiry, kill every process in
the watched command's session (POSIX) or its process tree (Windows), and exit
124; otherwise exit with the command's own status. The bound is a failure
bound surfaced to a human -- no test's correctness depends on it.

Exit status (the coreutils `timeout` convention):
    124  the bound fired and the tree was torn down
    125  the watchdog itself failed (`ps` missing, a kill refused, the bound
         not a finite number of seconds above zero, ...)
    126  the command was found but could not be executed
    127  the command was not found
      1  usage error (too few arguments)
      N  otherwise, the watched command's own exit status
"""

import math
import os
import shutil
import signal
import subprocess
import sys

WINDOWS = os.name == "nt"


def _die(message, code=125):
    """Report a watchdog failure and exit `code` -- 125 by default, distinct
    from 124 (the bound fired) and from the usage-error exit 1. The command's
    own failures to start take 127/126, as a shell reports them."""
    print(f"run-with-timeout: {message}", file=sys.stderr)
    sys.exit(code)


def _is_live(pid):
    """True if `pid` names a process that is not a zombie. `ps -o stat=`
    prints just the STAT column (`=` as the header suppresses it, a form
    both GNU/procps and BSD `ps` accept); a zombie's state starts with `Z`
    on both. A zombie already exited and released everything it held
    (including any pipe the watched command's descendants might have kept
    open) -- it persists only because its parent has not reaped it, which is
    not something a signal can change. `pid` having already been reaped
    entirely (no `ps` output at all) counts as not live too."""
    result = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], stdout=subprocess.PIPE, text=True, check=False)
    stat = result.stdout.strip()
    return bool(stat) and not stat.startswith("Z")


def _sigkill(pid):
    """SIGKILL `pid`, skipping one already gone. One this process may not
    signal ends the watchdog, reported: the kill loop would otherwise spin on
    it forever, and `main` would wait on it forever."""
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except PermissionError as e:
        _die(f"cannot SIGKILL pid {pid}: {e}")


def _session_members(sid):
    """Every pid `ps -A` lists whose session is `sid`. The session is read
    with `getsid`, because macOS's `pgrep`/`pkill` have no `-s` and its
    `ps -o sess` prints 0 for every process. A failing `ps` is reported and
    yields no members."""
    result = subprocess.run(
        ["ps", "-A", "-o", "pid="], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, check=False
    )
    if result.returncode != 0:
        print(f"run-with-timeout: `ps -A` failed (exit {result.returncode}): {result.stderr.strip()}", file=sys.stderr)
        return []
    members = []
    for pid in map(int, result.stdout.split()):
        try:
            if os.getsid(pid) == sid:
                members.append(pid)
        except ProcessLookupError:
            pass  # exited since `ps` listed it
        except PermissionError as e:
            # POSIX lets `getsid` refuse a process outside the caller's own
            # session; neither Linux (short of an LSM policy) nor macOS
            # (measured on CI) does. Whether it is in the watched session is
            # then unknowable, so it is skipped, and said so.
            print(f"run-with-timeout: cannot read pid {pid}'s session, skipping it: {e}", file=sys.stderr)
    return members


def kill_tree_posix(proc):
    """SIGKILL every process in the watched command's session, which
    `start_new_session=True` made `proc` the leader of. A session, unlike the
    process group `killpg` reaches, still holds a descendant that moved into
    a group of its own, as cosca's `.contain()` does; one that called
    `setsid()` itself has left it, and is not killed.

    The root goes first, directly by pid, so the caller's `proc.wait()` is
    bounded whatever the enumeration finds or fails to find; the loop skips
    it, since that `proc.wait()` is its reap. The loop repeats until no LIVE
    member is left, which catches one forked before its parent's SIGKILL
    landed. A zombie counts as gone: every SIGKILLed member becomes one until
    its own parent reaps it, which may be never. The loop ends because a
    SIGKILLed process cannot fork; a member in uninterruptible sleep (`D`)
    delays that until it wakes, and the CI job's `timeout-minutes` backstops
    one that never does."""
    _sigkill(proc.pid)
    while True:
        live = [pid for pid in _session_members(proc.pid) if pid != proc.pid and _is_live(pid)]
        if not live:
            return
        for pid in live:
            _sigkill(pid)


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
    try:
        seconds = float(argv[1])
    except ValueError:
        seconds = math.nan
    # `float` also accepts `inf` and `nan`, which would never fire, and a
    # bound at or below zero fires before the command can do anything.
    if not (math.isfinite(seconds) and seconds > 0):
        _die(f"the bound must be a finite number of seconds greater than zero, not {argv[1]!r}")
    command = argv[2:]
    if not WINDOWS and shutil.which("ps") is None:
        _die("`ps` is not on PATH, and expiry needs it to find the session to kill")

    # POSIX: a new session, which is what the kill on expiry goes by. Windows
    # has no equivalent -- CPython's `Popen` silently ignores the keyword there
    # rather than rejecting it, but it is left out anyway, since the kill on
    # that platform goes by process tree instead.
    kwargs = {} if WINDOWS else {"start_new_session": True}
    # A command that never starts is not the watched command's exit status --
    # it has none -- so report it the way a shell does rather than letting the
    # OSError escape as a traceback on exit 1.
    try:
        proc = subprocess.Popen(command, **kwargs)
    except FileNotFoundError as e:
        _die(f"cannot run {command[0]}: {e.strerror}", 127)
    except PermissionError as e:
        _die(f"cannot run {command[0]}: {e.strerror}", 126)
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
