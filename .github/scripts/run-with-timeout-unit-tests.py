#!/usr/bin/env python3
"""Deterministic unit coverage of `kill_tree_windows`'s LOGIC: the exact
`taskkill` argv, that a failed `taskkill` surfaces its stderr, and that
`proc.kill()` runs regardless -- by faking `subprocess.run` and `proc`, none
of this touches a real process or `taskkill`, so it runs on every OS
(including this Linux host) rather than needing a Windows runner. The real
end-to-end kill against an actual process tree is covered separately, on
the Windows CI leg only, by `run-with-timeout-windows-tests.py`.

Run directly: `python3 -m unittest run-with-timeout-unit-tests.py -v`.
"""

import importlib.util
import io
import sys
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest.mock import MagicMock, patch

# Loading the script under test through importlib would otherwise compile it
# into .github/scripts/__pycache__/, an untracked directory a local run leaves
# behind for someone to commit.
sys.dont_write_bytecode = True

SCRIPT_PATH = Path(__file__).resolve().parent / "run-with-timeout.py"


def _load_run_with_timeout():
    spec = importlib.util.spec_from_file_location("run_with_timeout", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


run_with_timeout = _load_run_with_timeout()


class FakeProc:
    """Stands in for the `Popen` handle `kill_tree_windows` receives: only
    `pid` and `kill()` are ever touched."""

    def __init__(self, pid=4242):
        self.pid = pid
        self.kill = MagicMock(name="proc.kill")


def _fake_run(returncode, stderr=b""):
    return MagicMock(return_value=MagicMock(returncode=returncode, stderr=stderr))


class KillTreeWindowsTests(unittest.TestCase):
    def test_argv_is_exact(self):
        proc = FakeProc(pid=4242)
        fake_run = _fake_run(returncode=0)
        with patch.object(run_with_timeout.subprocess, "run", fake_run):
            run_with_timeout.kill_tree_windows(proc)
        (argv,), _kwargs = fake_run.call_args
        self.assertEqual(argv, ["taskkill", "/T", "/F", "/PID", "4242"])

    def test_taskkill_failure_prints_stderr_and_still_kills(self):
        proc = FakeProc()
        fake_run = _fake_run(returncode=1, stderr=b"Access is denied.")
        captured = io.StringIO()
        with patch.object(run_with_timeout.subprocess, "run", fake_run), redirect_stderr(captured):
            run_with_timeout.kill_tree_windows(proc)
        self.assertIn("Access is denied.", captured.getvalue())
        proc.kill.assert_called_once()

    def test_taskkill_success_still_kills(self):
        # The backstop in the script is unconditional: `proc.kill()` is not
        # inside the `if result.returncode != 0:` branch, so it must run
        # even when `taskkill` itself reports success.
        proc = FakeProc()
        fake_run = _fake_run(returncode=0)
        with patch.object(run_with_timeout.subprocess, "run", fake_run):
            run_with_timeout.kill_tree_windows(proc)
        proc.kill.assert_called_once()


if __name__ == "__main__":
    unittest.main()
