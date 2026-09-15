#!/usr/bin/env python3
"""End-to-end coverage of run-with-timeout.py on Windows, against a REAL
process tree and REAL `taskkill` -- what `run-with-timeout-unit-tests.py`
fakes out. Windows-only: `taskkill` and the Win32 process APIs this uses have
no POSIX equivalent, and `run-with-timeout-posix-tests.py` covers the POSIX
kill path the same way.

The first case runs the whole script under a real bound and checks its exit
code, where a bound that fires early cannot hide a defect. The second builds
a launcher and its grandchild, blocks until the launcher reports the
grandchild's pid, and only then calls `kill_tree_windows` directly, so nothing
races the watchdog's timer. Every process here sleeps for 2**31 seconds and
cannot exit on its own.

Run directly: `python3 -B run-with-timeout-windows-tests.py`.
"""

import ctypes
import importlib.util
import os
import subprocess
import sys
from pathlib import Path

WINDOWS = os.name == "nt"

HERE = Path(__file__).resolve().parent
WATCHDOG = HERE / "run-with-timeout.py"

SLEEP_CODE = "import time; time.sleep(2**31)"
# Joined with `;` rather than newlines, and built without any embedded
# double quotes, so `subprocess.list2cmdline` never needs to escape
# anything inside this single argv token.
LAUNCHER_CODE = (
    "import subprocess, sys; "
    f"gc = subprocess.Popen([sys.executable, '-c', '{SLEEP_CODE}']); "
    "print(gc.pid, flush=True); "
    "gc.wait()"
)

# run-with-timeout.py's own timeout exit code (see `main()`'s `return 124`).
TIMEOUT_EXIT_CODE = 124

STILL_ACTIVE = 259
PROCESS_TERMINATE = 0x0001
PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
SYNCHRONIZE = 0x00100000
WAIT_OBJECT_0 = 0x0
WAIT_TIMEOUT = 0x102

# A failure bound surfaced to a human -- if the grandchild is still alive
# this long after `kill_tree_windows` returned, the kill silently didn't
# work, and that IS the thing under test. `WaitForSingleObject` blocks on the
# grandchild's real termination event and returns the instant it fires,
# well before this bound in the success case.
GRANDCHILD_DEATH_TIMEOUT_MS = 30_000


def _load_run_with_timeout():
    spec = importlib.util.spec_from_file_location("run_with_timeout", WATCHDOG)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


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
    kernel32.TerminateProcess.restype = ctypes.c_int
    kernel32.TerminateProcess.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
    kernel32.CloseHandle.restype = ctypes.c_int
    kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
    return kernel32


def _open_grandchild_handle(kernel32, pid):
    # Opened by PID once, up front, before the kill -- the returned HANDLE
    # then identifies this one kernel process object for the rest of the
    # script, so a later PID reuse can't be mistaken for the same process
    # staying alive, and the cleanup below can't terminate a stranger. The
    # grandchild cannot exit on its own and nothing kills it before this
    # call, so the PID still names it here.
    handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE | PROCESS_TERMINATE, False, pid)
    if handle is None:
        raise OSError(f"OpenProcess({pid}) failed: {ctypes.get_last_error()}")
    return handle


def _assert_grandchild_died(kernel32, handle, pid):
    """Blocks on the grandchild's own process handle: a real wait on its
    termination event, not a poll loop. Returns the instant the object is
    signalled (the process has actually exited), or WAIT_TIMEOUT once
    `GRANDCHILD_DEATH_TIMEOUT_MS` elapses -- see that constant's comment
    for why the timeout there is a failure bound, not a synchronisation
    hack."""
    result = kernel32.WaitForSingleObject(handle, GRANDCHILD_DEATH_TIMEOUT_MS)
    if result == WAIT_TIMEOUT:
        raise AssertionError(
            f"grandchild pid {pid} was still alive {GRANDCHILD_DEATH_TIMEOUT_MS}ms after kill_tree_windows returned"
        )
    if result != WAIT_OBJECT_0:
        raise OSError(f"WaitForSingleObject({pid}) returned {result}: {ctypes.get_last_error()}")

    exit_code = ctypes.c_ulong()
    if not kernel32.GetExitCodeProcess(handle, ctypes.byref(exit_code)):
        raise OSError(f"GetExitCodeProcess({pid}) failed: {ctypes.get_last_error()}")
    if exit_code.value == STILL_ACTIVE:
        raise AssertionError(f"grandchild pid {pid} reports STILL_ACTIVE after WaitForSingleObject signalled it dead")


def _terminate_if_alive(kernel32, handle, pid):
    # A zero-timeout wait is a status query: WAIT_TIMEOUT means still alive.
    if kernel32.WaitForSingleObject(handle, 0) == WAIT_TIMEOUT and not kernel32.TerminateProcess(handle, 1):
        raise OSError(f"TerminateProcess({pid}) failed: {ctypes.get_last_error()}")


def test_timeout_exit_code():
    status = subprocess.run([sys.executable, str(WATCHDOG), "1", sys.executable, "-c", SLEEP_CODE]).returncode
    if status != TIMEOUT_EXIT_CODE:
        raise AssertionError(f"expected the watchdog's timeout exit code {TIMEOUT_EXIT_CODE}, got {status}")
    print("ok   - reports the script's own timeout exit code")


def test_kills_the_process_tree(kernel32, run_with_timeout):
    launcher = subprocess.Popen([sys.executable, "-c", LAUNCHER_CODE], stdout=subprocess.PIPE, text=True)
    try:
        # A blocking read: it returns once the launcher has created the
        # grandchild and printed its pid.
        grandchild_pid = int(launcher.stdout.readline())
        handle = _open_grandchild_handle(kernel32, grandchild_pid)
        try:
            run_with_timeout.kill_tree_windows(launcher)
            launcher.wait()
            _assert_grandchild_died(kernel32, handle, grandchild_pid)
        finally:
            _terminate_if_alive(kernel32, handle, grandchild_pid)
            kernel32.CloseHandle(handle)
    finally:
        launcher.kill()  # a no-op once `wait()` has seen it exit
        launcher.wait()
    print("ok   - kills a real Windows process tree via taskkill and tears the grandchild down")


def main():
    if not WINDOWS:
        sys.exit("run-with-timeout-windows-tests.py only runs on Windows")
    test_timeout_exit_code()
    test_kills_the_process_tree(_kernel32(), _load_run_with_timeout())


if __name__ == "__main__":
    main()
