#!/usr/bin/env python3
"""Real-process coverage of run-with-timeout.py's POSIX kill path.

Each test forks a process tree, blocks until every process in it has reported
ready over a pipe, and only then calls the function under test, so nothing
races a timer. Every long-lived process is `sleep 2147483647`, which cannot
exit on its own; each is killed by pid if a test leaves it alive.

Run by `run-with-timeout-tests.sh`.
"""

import importlib.util
import os
import signal
import subprocess
import traceback
import unittest
from pathlib import Path

SCRIPT_PATH = Path(__file__).resolve().parent / "run-with-timeout.py"
FOREVER = ["sleep", "2147483647"]


def _load_run_with_timeout():
    spec = importlib.util.spec_from_file_location("run_with_timeout", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


run_with_timeout = _load_run_with_timeout()


def _fork(body):
    """Fork a child that runs `body`, which must exec or exit, and never
    returns into the test runner."""
    pid = os.fork()
    if pid == 0:
        try:
            body()
        except BaseException:
            traceback.print_exc()
        os._exit(1)
    return pid


def _ps_stat(pid):
    return subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], stdout=subprocess.PIPE, text=True).stdout.strip()


class Lifeline:
    """A pipe whose write end exactly one tracked process holds, so it has a
    writer exactly while that process is alive. Unlike a pid, this cannot
    come to name a different process once the tracked one is reaped."""

    def __init__(self):
        self.read_end, self.write_end = os.pipe()

    def alive(self):
        os.set_blocking(self.read_end, False)
        try:
            return os.read(self.read_end, 1) != b""
        except BlockingIOError:
            return True


class Handshake:
    """One pipe every process in a tree reports `<name> <pid>` on, once it
    holds only the file descriptors it is meant to."""

    def __init__(self):
        self.read_end, self.write_end = os.pipe()

    def report(self, name):
        os.write(self.write_end, f"{name} {os.getpid()}\n".encode())

    def read(self, count):
        with os.fdopen(self.read_end) as lines:
            return dict(line.split() for line in (lines.readline() for _ in range(count)))


def _exec_forever(keep=None):
    """Exec `sleep` holding `keep`'s write end, if given. Every other
    descriptor here is close-on-exec, and stdio goes to /dev/null: a sleeper
    a failed test leaves behind must not hold open a pipe its caller reads."""
    devnull = os.open(os.devnull, os.O_RDWR)
    for fd in (0, 1, 2):
        os.dup2(devnull, fd)
    if keep is not None:
        os.set_inheritable(keep.write_end, True)
    os.execvp(FOREVER[0], FOREVER)


def _report_then_sleep(handshake, name, keep, drop=()):
    """Close every lifeline in `drop`, report, and exec `sleep` holding `keep`."""
    for lifeline in drop:
        os.close(lifeline.write_end)
    handshake.report(name)
    _exec_forever(keep)


class Root:
    """The watched command, forked into a session of its own as `main`'s
    `start_new_session=True` does; `kill_tree_posix` reads only `.pid`."""

    def __init__(self, body):
        def leader():
            os.setsid()
            body()

        self.pid = _fork(leader)
        self.reaped = False

    def wait(self):
        if not self.reaped:
            os.waitpid(self.pid, 0)
            self.reaped = True

    def kill(self):
        # Safe by pid until reaped: an unreaped child's pid cannot be reused.
        if not self.reaped:
            os.kill(self.pid, signal.SIGKILL)
            self.wait()


class KillTreePosixTests(unittest.TestCase):
    def build(self, root_body, handshake, lifelines, count):
        """Start the tree and return `{name: pid}` once all `count` processes
        have reported. This process drops its own write ends first, so a
        fixture that dies early surfaces as EOF rather than a hang."""
        root = Root(root_body)
        self.addCleanup(root.kill)
        for fd in [handshake.write_end, *(lifeline.write_end for lifeline in lifelines)]:
            os.close(fd)
        pids = {name: int(pid) for name, pid in handshake.read(count).items()}
        self.assertEqual(len(pids), count, f"the fixture did not report ready: {pids}")
        return root, pids

    def kill_if_alive(self, pid, lifeline):
        if lifeline.alive():
            os.kill(pid, signal.SIGKILL)

    def test_kills_a_member_that_moved_into_its_own_process_group(self):
        # The root forks C, which moves into a process group of its own and
        # stays in the session, as cosca's `.contain()` does to a child.
        handshake, c_life = Handshake(), Lifeline()

        def c_body():
            os.setpgid(0, 0)
            _report_then_sleep(handshake, "C", keep=c_life)

        def root_body():
            _fork(c_body)
            _exec_forever()

        root, pids = self.build(root_body, handshake, [c_life], count=1)
        c = pids["C"]
        self.addCleanup(self.kill_if_alive, c, c_life)
        self.assertEqual((os.getsid(c), os.getpgid(c)), (root.pid, c), "C must be in the session, in its own group")

        run_with_timeout.kill_tree_posix(root)
        root.wait()
        self.assertFalse(c_life.alive(), f"session member {c} survived")

    def test_returns_while_a_killed_member_stays_an_unreaped_zombie(self):
        # The root forks P; P forks C, then leaves the session with setsid()
        # and never reaps C. Killed, C stays a zombie the session still lists
        # for as long as P lives, so the loop must not wait for its reap. A
        # loop that does spins forever here: a regression hangs this test
        # rather than failing it, and the CI job's `timeout-minutes` ends it.
        handshake, c_life, p_life = Handshake(), Lifeline(), Lifeline()

        def p_body():
            _fork(lambda: _report_then_sleep(handshake, "C", keep=c_life, drop=[p_life]))
            os.setsid()
            signal.signal(signal.SIGCHLD, signal.SIG_DFL)  # an inherited SIG_IGN would reap C
            _report_then_sleep(handshake, "P", keep=p_life, drop=[c_life])

        def root_body():
            _fork(p_body)
            _exec_forever()

        root, pids = self.build(root_body, handshake, [c_life, p_life], count=2)
        c, p = pids["C"], pids["P"]
        self.addCleanup(self.kill_if_alive, c, c_life)
        self.addCleanup(self.kill_if_alive, p, p_life)
        self.assertEqual((os.getsid(c), os.getsid(p)), (root.pid, p), "C must be in the session, P outside it")

        run_with_timeout.kill_tree_posix(root)
        root.wait()
        self.assertFalse(c_life.alive(), f"session member {c} survived")
        self.assertTrue(p_life.alive(), f"{p} left the session, so it is not the watchdog's to kill")
        # C's pid is pinned while P, its parent, lives.
        self.assertTrue(_ps_stat(c).startswith("Z"), f"C ({c}) must be an unreaped zombie, got {_ps_stat(c)!r}")


class IsLiveTests(unittest.TestCase):
    def test_tells_a_live_process_from_a_zombie(self):
        self.assertTrue(run_with_timeout._is_live(os.getpid()))

        pid = _fork(lambda: os._exit(0))
        try:
            # Returns once the child has exited, without reaping it: it stays
            # a zombie until the `waitpid` below.
            os.waitid(os.P_PID, pid, os.WEXITED | os.WNOWAIT)
            self.assertTrue(_ps_stat(pid).startswith("Z"), f"the fixture must be a zombie, got {_ps_stat(pid)!r}")
            self.assertFalse(run_with_timeout._is_live(pid))
        finally:
            os.waitpid(pid, 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
