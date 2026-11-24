"""Usage: stream.py <passes of 1 job> <passes of 4 jobs> <passes of 16 jobs> <passes of 1 job under log noise>

Claim measured: whether a live `log stream` drops UserEventAgent's `Registered StartCalendarInterval` lines when a
job with 180 entries (`*/15 9-17 * * 1-5` as goetia compiles it: {Weekday, Hour, Minute}) is bootstrapped. Each pass
attaches a stream as goetia's launchd test does (`sh -c 'exec log stream ... 2>&1'`, a canary logged again every
100 ms until it comes back), bootstraps the job(s), takes the ground truth from `log show` (polled until every
entry has its line) and `launchctl print` (the entry count), then logs a sentinel again and again until the stream
returns it, and counts the distinct Registered lines the stream returned by then.

Platform: GitHub macos-latest runner, as root, real clock. Run only on a throwaway machine.
Expected output: one `PASS` line per pass with the three counts, `COMPLETE` or `DROPPED` and the number of
`Messages dropped` markers; a final tally.
"""

from __future__ import annotations

import queue
import re
import subprocess
import threading
import time

from common import REGISTERED, Jobs, Clock, install_cleanup, log_show, platform_header, run, say

ENTRIES = [{"Weekday": day, "Hour": hour, "Minute": minute} for day in range(1, 6) for hour in range(9, 18) for minute in (0, 15, 30, 45)]
RELOG_SECONDS = 0.1
FAILURE_BOUND = 180.0
STREAM_COMMAND = "exec log stream --level debug --style compact --predicate 'eventMessage CONTAINS \"StartCalendarInterval\"' 2>&1"
DESCRIPTOR = re.compile(r"^\s*(\S+)\.(\d+) => \{")


class Stream:
    def __init__(self) -> None:
        self.process = subprocess.Popen(["/bin/sh", "-c", STREAM_COMMAND], stdout=subprocess.PIPE, text=True)
        self.lines: queue.Queue[str] = queue.Queue()
        self.thread = threading.Thread(target=self._read, daemon=True)
        self.thread.start()
        self.seen: list[str] = []

    def _read(self) -> None:
        assert self.process.stdout is not None
        for line in self.process.stdout:
            self.lines.put(line.rstrip("\n"))
        self.lines.put("<<stream ended>>")

    def next_line(self, timeout: float) -> str | None:
        try:
            line = self.lines.get(timeout=timeout)
        except queue.Empty:
            return None
        self.seen.append(line)
        return line

    def until_logged_back(self, marker: str) -> bool:
        """Logs `marker` again every RELOG_SECONDS until the stream returns it; False at the failure bound."""
        start = time.monotonic()
        logged = 0.0
        while True:
            if time.monotonic() - logged >= RELOG_SECONDS:
                run(["logger", marker])
                logged = time.monotonic()
            line = self.next_line(max(RELOG_SECONDS - (time.monotonic() - logged), 0.001))
            if line is not None and marker in line:
                return True
            if line == "<<stream ended>>" or time.monotonic() - start > FAILURE_BOUND:
                say(f"MARKER NOT RETURNED: {marker}")
                return False

    def close(self) -> None:
        self.process.terminate()
        self.process.wait()

    def registered(self, labels: set[str]) -> set[tuple[str, str]]:
        found = set()
        for line in self.seen:
            match = REGISTERED.search(line)
            if match and match.group(1) in labels:
                found.add((match.group(1), match.group(2)))
        return found

    def dropped_markers(self) -> int:
        return sum("Messages dropped" in line for line in self.seen)


def ground_truth(tag: str, labels: set[str]) -> set[tuple[str, str]]:
    """The (label, id) of every Registered line `log show` returns, polled until each label has all its entries."""
    start = time.monotonic()
    while True:
        found = set()
        for line in log_show(f"goetia.probe.{tag}.", window="5m"):
            match = REGISTERED.search(line)
            if match and match.group(1) in labels:
                found.add((match.group(1), match.group(2)))
        if len(found) >= len(labels) * len(ENTRIES) or time.monotonic() - start > FAILURE_BOUND:
            return found


def print_count(labels: set[str]) -> int:
    total = 0
    for label in labels:
        for line in run(["launchctl", "print", f"system/{label}"]).stdout.splitlines():
            match = DESCRIPTOR.match(line)
            if match and match.group(1) == label:
                total += 1
    return total


def one_pass(jobs: Jobs, index: int, count: int, noisy: bool) -> bool:
    tag = f"s{index}"
    labels = {f"goetia.probe.{tag}.j{n}" for n in range(count)}
    stream = Stream()
    noise = None
    try:
        stream.next_line(FAILURE_BOUND)  # the stream's own header, printed before it is subscribed
        canary = f"StartCalendarInterval canary {tag}"
        attached = stream.until_logged_back(canary)
        if noisy:
            noise_file = jobs.work / "noise.txt"
            noise_file.write_text("StartCalendarInterval noise\n" * 20000)
            noise = subprocess.Popen(["logger", "-f", str(noise_file)])
        for label in sorted(labels):
            jobs.add(label, ENTRIES)
        for label in sorted(labels):
            jobs.bootstrap(label)
        truth = ground_truth(tag, labels)
        printed = print_count(labels)
        sentinel_a = stream.until_logged_back(f"StartCalendarInterval sentinel {tag} a")
        first = len(stream.registered(labels))
        sentinel_b = stream.until_logged_back(f"StartCalendarInterval sentinel {tag} b")
        second = len(stream.registered(labels))
        expected = count * len(ENTRIES)
        complete = attached and sentinel_a and sentinel_b and len(truth) == expected and first == expected and second == expected
        say(
            f"PASS {index} jobs={count} noise={noisy} expected={expected} print={printed} logshow={len(truth)} "
            f"stream={first} stream_later={second} missing_from_stream={len(truth - stream.registered(labels))} "
            f"drop_markers={stream.dropped_markers()} {'COMPLETE' if complete else 'DROPPED-OR-INCOMPLETE'}"
        )
        return complete
    finally:
        stream.close()
        if noise is not None:
            noise.wait()
        for label in sorted(labels):
            jobs.drop(label)


def main() -> None:
    import sys

    plan = [(1, False)] * int(sys.argv[1]) + [(4, False)] * int(sys.argv[2]) + [(16, False)] * int(sys.argv[3])
    plan += [(1, True)] * int(sys.argv[4])
    platform_header()
    jobs, clock = Jobs(), Clock()
    install_cleanup(jobs, clock)
    results = [one_pass(jobs, index, count, noisy) for index, (count, noisy) in enumerate(plan, start=1)]
    say(f"TALLY: {sum(results)} of {len(results)} passes complete")


if __name__ == "__main__":
    main()
