#!/usr/bin/env python3
"""End-to-end coverage of `kill_tree_windows` against a REAL process tree
and REAL `taskkill` -- what `run-with-timeout-unit-tests.py` fakes out.
Windows-only: `taskkill` and the Win32 process APIs this uses have no
POSIX equivalent, and `run-with-timeout-tests.sh` already covers the
POSIX kill path the same way, against real processes. Not merged
into that suite because bash has no way to call `OpenProcess` /
`WaitForSingleObject`.

The watched command is a Python process that spawns a grandchild Python
process sleeping for 2**31 seconds and then blocks on that grandchild:
neither process can exit on its own, so the watchdog's bound firing is
provably the only way this script can ever finish -- nothing here races a
short sleep against that bound.

Run directly: `python3 run-with-timeout-windows-tests.py`.
"""

import ctypes
import os
import subprocess
import sys
from pathlib import Path

WINDOWS = os.name == "nt"

HERE = Path(__file__).resolve().parent
WATCHDOG = HERE / "run-with-timeout.py"

# Joined with `;` rather than newlines, and built without any embedded
# double quotes, so `subprocess.list2cmdline` never needs to escape
# anything inside this single argv token.
LAUNCHER_CODE = (
    "import subprocess, sys; "
    "gc = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(2**31)']); "
    "print(gc.pid, flush=True); "
    "gc.wait()"
)

# run-with-timeout.py's own timeout exit code (see `main()`'s `return 124`).
TIMEOUT_EXIT_CODE = 124

STILL_ACTIVE = 259
PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
SYNCHRONIZE = 0x00100000
WAIT_OBJECT_0 = 0x0
WAIT_TIMEOUT = 0x102

# A failure bound surfaced to a human -- if the grandchild is still alive
# this long after the watchdog's own bound fired, the kill silently didn't
# work, and that IS the thing under test. This is not a race against
# anything this script itself starts: `WaitForSingleObject` blocks on the
# grandchild's real termination event and returns the instant it fires,
# well before this bound in the success case.
GRANDCHILD_DEATH_TIMEOUT_MS = 30_000


def _kernel32():
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    # ctypes defaults every foreign function's restype to a 32-bit c_int,
    # which would truncate a 64-bit HANDLE on Win64 -- spell out the real
    # signatures instead of trusting the default.
    kernel32.OpenProcess.restype = ctypes.c_void_p
    kernel32.OpenProcess.argtypes = [ctypes.c_uint32, ctypes.c_int, ctypes.c_uint32]
    kernel32.WaitForSingleObject.restype = ctypes.c_uint32
    kernel32.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
    kernel32.GetExitCodeProcess.restype = ctypes.c_int
    kernel32.GetExitCodeProcess.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_ulong)]
    kernel32.CloseHandle.restype = ctypes.c_int
    kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
    return kernel32


def _open_grandchild_handle(kernel32, pid):
    # Opened by PID once, up front, before the kill -- the returned HANDLE
    # then identifies this one kernel process object for the rest of the
    # script, so a later PID reuse (however unlikely in this short a
    # window) can't be mistaken for the same process staying alive. This is
    # why this test uses OpenProcess/WaitForSingleObject rather than
    # re-querying `tasklist /FI "PID eq <n>"` after the kill: `tasklist`
    # only ever has the PID to go on, so it cannot tell "still alive" from
    # "a new process happens to have the same PID" -- ctypes pins identity
    # via the handle instead, and needs no subprocess spawn or text parsing
    # to do it.
    handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, False, pid)
    if handle is None:
        raise OSError(f"OpenProcess({pid}) failed: {ctypes.get_last_error()}")
    return handle


def _assert_grandchild_died(kernel32, handle, pid):
    """Blocks on the grandchild's own process handle: a real wait on its
    termination event, not a poll loop. Returns the instant the object is
    signalled (the process has actually exited), or WAIT_TIMEOUT once
    `GRANDCHILD_DEATH_TIMEOUT_MS` elapses -- see that constant's docstring
    for why the timeout there is a failure bound, not a synchronisation
    hack."""
    result = kernel32.WaitForSingleObject(handle, GRANDCHILD_DEATH_TIMEOUT_MS)
    if result == WAIT_TIMEOUT:
        raise AssertionError(f"grandchild pid {pid} was still alive {GRANDCHILD_DEATH_TIMEOUT_MS}ms after the bound fired")
    if result != WAIT_OBJECT_0:
        raise OSError(f"WaitForSingleObject({pid}) returned {result}: {ctypes.get_last_error()}")

    exit_code = ctypes.c_ulong()
    if not kernel32.GetExitCodeProcess(handle, ctypes.byref(exit_code)):
        raise OSError(f"GetExitCodeProcess({pid}) failed: {ctypes.get_last_error()}")
    if exit_code.value == STILL_ACTIVE:
        raise AssertionError(f"grandchild pid {pid} reports STILL_ACTIVE after WaitForSingleObject signalled it dead")


def main():
    if not WINDOWS:
        sys.exit("run-with-timeout-windows-tests.py only runs on Windows")

    kernel32 = _kernel32()
    command = [sys.executable, str(WATCHDOG), "2", sys.executable, "-c", LAUNCHER_CODE]
    proc = subprocess.Popen(command, stdout=subprocess.PIPE, text=True)

    # A real blocking read on the launcher's stdout pipe: it returns the
    # instant the launcher's `print(gc.pid, flush=True)` lands, not after a
    # chosen delay.
    grandchild_pid = int(proc.stdout.readline().strip())
    handle = _open_grandchild_handle(kernel32, grandchild_pid)
    try:
        # No timeout of our own choosing here either: this blocks until the
        # watchdog process itself exits, which is bounded by the watchdog's
        # OWN internal bound (2s, passed above) firing -- the CI job's
        # overall `timeout-minutes` is the human-facing backstop if that
        # bound itself were ever to hang.
        status = proc.wait()
        if status != TIMEOUT_EXIT_CODE:
            raise AssertionError(f"expected the watchdog's timeout exit code {TIMEOUT_EXIT_CODE}, got {status}")

        _assert_grandchild_died(kernel32, handle, grandchild_pid)
    finally:
        kernel32.CloseHandle(handle)

    print("ok   - kills a real Windows process tree via taskkill and tears the grandchild down")


if __name__ == "__main__":
    main()
