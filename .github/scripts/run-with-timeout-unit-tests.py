#!/usr/bin/env python3
"""Deterministic unit coverage of run-with-timeout.py's kill LOGIC on both
platforms. Every process, `ps`, `taskkill`, `getsid` and `kill` is faked, so
this runs on every OS, including this Linux host. Real process trees are
covered by `run-with-timeout-tests.sh` (POSIX) and
`run-with-timeout-windows-tests.py` (the Windows CI leg).

Run from this directory: `python3 -B -m unittest run-with-timeout-unit-tests -v`.
"""

import importlib.util
import io
import re
import subprocess
import unittest
from contextlib import ExitStack, redirect_stderr
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

SCRIPT_PATH = Path(__file__).resolve().parent / "run-with-timeout.py"


def _load_run_with_timeout():
    spec = importlib.util.spec_from_file_location("run_with_timeout", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


run_with_timeout = _load_run_with_timeout()

# Stands in for `signal.SIGKILL`, which Windows does not define.
SIGKILL = 9
ROOT = 100
SECONDS = 5


def _session(*members):
    """`ROOT`'s session holding `members`, plus two processes outside it."""
    return {ROOT: ROOT, **{pid: ROOT for pid in members}, 1: 1, 200: 1}


class FakeHost:
    """A process table behind `kill_tree_posix`'s touch points: `ps` (through
    `subprocess.run`), `os.getsid` and `os.kill`. A SIGKILLed process becomes
    a zombie that nothing reaps and `ps` still lists. A SIGKILL here takes
    effect at once, so signalling a LIVE pid twice can only mean the kill
    loop is spinning: that fails the test instead of hanging it. Signalling a
    zombie again is allowed, and is what a later pass legitimately does --
    `kill_tree_posix` signals straight off the enumeration rather than
    checking each pid first."""

    def __init__(self, sessions):
        self.sid = dict(sessions)
        self.stat = {pid: "S" for pid in sessions}
        self.kills = []
        self.ops = []  # ("kill" | "check", pid), in call order
        self.ps_failure = None  # stderr of a failing `ps -A`
        self.gone_before_getsid = set()  # listed by `ps -A`, gone by `getsid`
        self.gone_before_kill = set()  # in the session, gone by `kill`
        self.forks_on_kill = {}  # pid -> the child it forked before SIGKILL landed
        self.refuses_kill = set()
        self.refuses_getsid = set()

    def run(self, argv, **_kwargs):
        if argv == ["ps", "-A", "-o", "pid="]:
            if self.ps_failure is not None:
                return subprocess.CompletedProcess(argv, 1, "", self.ps_failure)
            listed = sorted(set(self.sid) | self.gone_before_getsid)
            return subprocess.CompletedProcess(argv, 0, "".join(f"{pid}\n" for pid in listed), "")
        if argv[:3] == ["ps", "-o", "stat="]:
            self.ops.append(("check", int(argv[-1])))
            stat = self.stat.get(int(argv[-1]))
            if stat is None:
                return subprocess.CompletedProcess(argv, 1, "", "")
            return subprocess.CompletedProcess(argv, 0, f"{stat}\n", "")
        if argv[0] in ("pgrep", "pkill") and "-s" in argv:
            # What macOS's pgrep/pkill answered when measured on CI.
            return subprocess.CompletedProcess(argv, 2, "", f"{argv[0]}: illegal option -- s")
        raise AssertionError(f"unexpected command: {argv}")

    def getsid(self, pid):
        if pid in self.refuses_getsid:
            raise PermissionError(1, "Operation not permitted")
        if pid not in self.sid:
            raise ProcessLookupError(3, "No such process")
        return self.sid[pid]

    def kill(self, pid, sig):
        if (pid, sig) in self.kills and self.stat.get(pid) != "Z":
            raise AssertionError(f"live pid {pid} signalled twice: the kill loop is spinning")
        self.kills.append((pid, sig))
        self.ops.append(("kill", pid))
        if pid in self.refuses_kill:
            raise PermissionError(1, "Operation not permitted")
        if pid in self.gone_before_kill or pid not in self.sid:
            self.sid.pop(pid, None)
            self.stat.pop(pid, None)
            raise ProcessLookupError(3, "No such process")
        if pid in self.forks_on_kill:
            child = self.forks_on_kill.pop(pid)
            self.sid[child] = self.sid[pid]
            self.stat[child] = "S"
        self.stat[pid] = "Z"

    def killed(self):
        """The distinct pids signalled, since a zombie may be signalled on
        more than one pass."""
        return sorted({pid for pid, _sig in self.kills})

    def installed(self):
        stack = ExitStack()
        stack.enter_context(patch.object(run_with_timeout.subprocess, "run", self.run))
        stack.enter_context(patch.object(run_with_timeout.os, "getsid", self.getsid, create=True))
        stack.enter_context(patch.object(run_with_timeout.os, "kill", self.kill))
        stack.enter_context(patch.object(run_with_timeout, "signal", SimpleNamespace(SIGKILL=SIGKILL), create=True))
        return stack


class KillTreePosixTests(unittest.TestCase):
    def kill_tree(self, host):
        captured = io.StringIO()
        with host.installed(), redirect_stderr(captured):
            run_with_timeout.kill_tree_posix(SimpleNamespace(pid=ROOT))
        return captured.getvalue()

    def test_a_failing_ps_still_kills_the_root_and_reports_why(self):
        host = FakeHost(_session(101))
        host.ps_failure = "ps: cannot enumerate"
        stderr = self.kill_tree(host)
        self.assertIn((ROOT, SIGKILL), host.kills)
        self.assertIn("ps: cannot enumerate", stderr)

    def test_kills_the_root_first_then_every_session_member_and_nothing_else(self):
        host = FakeHost(_session(101, 102))
        self.kill_tree(host)
        self.assertEqual(host.kills[:1], [(ROOT, SIGKILL)])
        self.assertEqual(host.killed(), [ROOT, 101, 102])

    def test_every_member_is_signalled_before_any_liveness_check(self):
        # A pid checked and only then killed can be reaped and its number
        # reused in between, and under `sudo` the SIGKILL that follows is
        # unrestricted. Liveness decides only whether another pass is needed,
        # so no check may stand between `getsid` naming a member and its
        # kill.
        host = FakeHost(_session(101, 102))
        self.kill_tree(host)
        first_check = next((i for i, (op, _pid) in enumerate(host.ops) if op == "check"), None)
        self.assertIsNotNone(first_check, "nothing decided whether another pass was needed")
        signalled_first = sorted(pid for op, pid in host.ops[:first_check] if op == "kill")
        self.assertEqual(signalled_first, [ROOT, 101, 102])

    def test_a_member_forked_before_its_parent_died_is_killed_on_the_next_pass(self):
        host = FakeHost(_session(101))
        host.forks_on_kill[101] = 103
        self.kill_tree(host)
        self.assertEqual(host.killed(), [ROOT, 101, 103])

    def test_a_process_that_vanishes_mid_teardown_is_skipped(self):
        host = FakeHost(_session(101, 102))
        host.gone_before_getsid.add(150)
        host.gone_before_kill.add(101)
        self.kill_tree(host)
        self.assertIn(102, host.killed())
        self.assertNotIn(150, host.killed())

    def test_a_process_whose_session_it_may_not_read_is_reported_and_skipped(self):
        host = FakeHost(_session(101))
        host.refuses_getsid.add(200)
        stderr = self.kill_tree(host)
        self.assertRegex(stderr, r"run-with-timeout: .*\b200\b")
        self.assertEqual(host.killed(), [ROOT, 101])


class FakePopen:
    """A watched command that outlives the bound: the bounded `wait` times
    out, and an unbounded one returns as if SIGKILLed."""

    def __init__(self, pid):
        self.pid = pid
        self.waits = []

    def wait(self, timeout=None):
        self.waits.append(timeout)
        if timeout is not None:
            raise subprocess.TimeoutExpired("command", timeout)
        return -SIGKILL


class ExitingPopen:
    """A watched command that exits with `code` inside the bound."""

    def __init__(self, code):
        self.pid = ROOT
        self.code = code
        self.waits = []

    def wait(self, timeout=None):
        self.waits.append(timeout)
        return self.code


class MainPosixTests(unittest.TestCase):
    def run_main(self, popen, ps_path, host=None):
        self.stderr = io.StringIO()
        with ExitStack() as stack:
            if host is not None:
                stack.enter_context(host.installed())
            stack.enter_context(patch.object(run_with_timeout, "WINDOWS", False))
            stack.enter_context(patch.object(run_with_timeout.subprocess, "Popen", popen))
            which = SimpleNamespace(which=lambda _name: ps_path)
            stack.enter_context(patch.object(run_with_timeout, "shutil", which, create=True))
            stack.enter_context(redirect_stderr(self.stderr))
            return run_with_timeout.main(["run-with-timeout.py", str(SECONDS), "command"])

    def test_a_missing_ps_fails_before_the_command_starts(self):
        popen = MagicMock(side_effect=AssertionError("the command was started"))
        with self.assertRaises(SystemExit) as raised:
            self.run_main(popen, ps_path=None)
        self.assertEqual(raised.exception.code, 125)
        self.assertRegex(self.stderr.getvalue(), r"^run-with-timeout: .*\bps\b")
        popen.assert_not_called()

    def test_a_process_it_cannot_kill_is_reported_and_the_root_is_not_waited_on(self):
        for refuser in (ROOT, 101):
            with self.subTest(refuser=refuser):
                host = FakeHost(_session(101))
                host.refuses_kill.add(refuser)
                proc = FakePopen(ROOT)
                with self.assertRaises(SystemExit) as raised:
                    self.run_main(lambda *_args, **_kwargs: proc, ps_path="/bin/ps", host=host)
                self.assertEqual(raised.exception.code, 125)
                self.assertRegex(self.stderr.getvalue(), rf"(?m)^run-with-timeout: .*\b{refuser}\b")
                self.assertEqual(proc.waits, [SECONDS])

    def run_main_argv(self, popen, argv, ps_path="/bin/ps"):
        """`run_main`, but for the argv-level failures that never reach a host."""
        self.stderr = io.StringIO()
        with ExitStack() as stack:
            stack.enter_context(patch.object(run_with_timeout, "WINDOWS", False))
            stack.enter_context(patch.object(run_with_timeout.subprocess, "Popen", popen))
            which = SimpleNamespace(which=lambda _name: ps_path)
            stack.enter_context(patch.object(run_with_timeout, "shutil", which, create=True))
            stack.enter_context(redirect_stderr(self.stderr))
            return run_with_timeout.main(argv)

    def test_a_bound_that_is_not_a_number_is_a_watchdog_failure(self):
        popen = MagicMock(side_effect=AssertionError("the command was started"))
        with self.assertRaises(SystemExit) as raised:
            self.run_main_argv(popen, ["run-with-timeout.py", "abc", "command"])
        self.assertEqual(raised.exception.code, 125)
        self.assertRegex(self.stderr.getvalue(), r"^run-with-timeout: .*abc")
        popen.assert_not_called()

    def test_a_bound_that_is_not_finite_and_positive_is_a_watchdog_failure(self):
        # `inf` and `nan` would never fire, silently disabling the watchdog;
        # `0` and `-1` would fire before the command could do anything.
        for bound in ("inf", "nan", "-1", "0"):
            with self.subTest(bound=bound):
                popen = MagicMock(side_effect=AssertionError("the command was started"))
                with self.assertRaises(SystemExit) as raised:
                    self.run_main_argv(popen, ["run-with-timeout.py", bound, "command"])
                self.assertEqual(raised.exception.code, 125)
                self.assertRegex(self.stderr.getvalue(), rf"^run-with-timeout: .*'{re.escape(bound)}'")
                popen.assert_not_called()

    def test_a_valid_bound_reaches_the_wait(self):
        proc = ExitingPopen(code=3)
        code = self.run_main_argv(lambda *_args, **_kwargs: proc, ["run-with-timeout.py", "0.5", "command"])
        self.assertEqual(code, 3)
        self.assertEqual(proc.waits, [0.5])

    def test_a_command_that_does_not_exist_reports_127(self):
        popen = MagicMock(side_effect=FileNotFoundError(2, "No such file or directory"))
        with self.assertRaises(SystemExit) as raised:
            self.run_main_argv(popen, ["run-with-timeout.py", str(SECONDS), "no-such-cmd"])
        self.assertEqual(raised.exception.code, 127)
        self.assertRegex(self.stderr.getvalue(), r"^run-with-timeout: .*no-such-cmd")

    def test_a_command_that_is_not_executable_reports_126(self):
        popen = MagicMock(side_effect=PermissionError(13, "Permission denied"))
        with self.assertRaises(SystemExit) as raised:
            self.run_main_argv(popen, ["run-with-timeout.py", str(SECONDS), "not-executable"])
        self.assertEqual(raised.exception.code, 126)
        self.assertRegex(self.stderr.getvalue(), r"^run-with-timeout: .*not-executable")


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
