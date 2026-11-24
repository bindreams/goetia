"""Usage: dst.py <lh|sc|bt|tr> <gap|foldA|foldB> <repetitions>

Claim measured: how launchd registers and fires StartCalendarInterval entries in a spring-forward gap and a
fall-back fold of zones other than America/New_York: Australia/Lord_Howe (lh, 30 min), America/Santiago (sc) and
Asia/Beirut (bt) (transitions at midnight), Antarctica/Troll (tr, 2 h). The model under test (goetia's
`src/backend/launchd/next.rs`, measured in New York): an entry that names its hour and falls in a gap fires once at
the gap's end; an entry with no hour skips the gap; in a fold, an entry with no date or a weekday fires at both
passes and one with a Day or Month and Day at the first only.

Platform: GitHub macos-latest runner, as root. Run only on a throwaway machine.
Expected output: per repetition, REGISTERED lines (the next fire of each entry), FIRES lines (UTC and zone-local
time of each run) and the Running/Rescheduling lines of the log.

Steps of a repetition: `gap`: register the day before (registration only), then 75 s before the transition and wait
for the control entry one minute after the gap's end. `foldA`: register in the first pass, wait for the control,
jump to the second pass and wait for the control again. `foldB`: register in the second pass, wait for the control.
The clock is put back after each repetition.
"""

from __future__ import annotations

import sys
import time
from datetime import datetime, timedelta, timezone

from common import (
    Clock,
    Jobs,
    install_cleanup,
    log_show,
    platform_header,
    registered_lines,
    run,
    say,
    use_zone,
    utc,
    wait_fires,
)

# zone key: (zone, gap, fold). Times are local (hour, minute); a date is (month, day, weekday, 0 = Sunday), 2027.
# gap: U is the transition's UTC instant; the local clock reads `start` just before it and `end` at it.
# fold: the local clock reads `f1` just before U and `f0` at it, so [f0, f1) is read twice; `mid` is the entries' time.
ZONES = {
    "lh": (
        "Australia/Lord_Howe",
        dict(U="2027-10-02T15:30:00", date=(10, 3, 0), start=(2, 0), end=(2, 30), mid=(2, 15)),
        dict(U="2027-04-03T15:00:00", date=(4, 4, 0), f0=(1, 30), f1=(2, 0), mid=(1, 45)),
    ),
    "sc": (
        "America/Santiago",
        dict(U="2027-09-05T04:00:00", date=(9, 5, 0), start=(0, 0), end=(1, 0), mid=(0, 30)),
        dict(U="2027-04-04T03:00:00", date=(4, 3, 6), f0=(23, 0), f1=(24, 0), mid=(23, 30)),
    ),
    "bt": (
        "Asia/Beirut",
        dict(U="2027-03-27T22:00:00", date=(3, 28, 0), start=(0, 0), end=(1, 0), mid=(0, 30)),
        dict(U="2027-10-30T21:00:00", date=(10, 30, 6), f0=(23, 0), f1=(24, 0), mid=(23, 30)),
    ),
    "tr": (
        "Antarctica/Troll",
        dict(U="2027-03-28T01:00:00", date=(3, 28, 0), start=(1, 0), end=(3, 0), mid=(2, 30)),
        dict(U="2027-10-31T01:00:00", date=(10, 31, 0), f0=(1, 0), f1=(3, 0), mid=(2, 30)),
    ),
}
MARGIN = timedelta(seconds=75)
FIRE_BOUND = 200.0


def minutes(hm: tuple[int, int]) -> int:
    return hm[0] * 60 + hm[1]


def civil(date: tuple[int, int, int], hm: tuple[int, int], delta: int = 0) -> dict[str, int]:
    """The Month, Day, Hour, Minute and Weekday (0 = Sunday) of a local time `delta` minutes from `hm` on `date`."""
    when = datetime(2027, date[0], date[1]) + timedelta(minutes=minutes(hm) + delta)
    return dict(Month=when.month, Day=when.day, Hour=when.hour, Minute=when.minute, Weekday=(when.weekday() + 1) % 7)


def keys(source: dict[str, int], *names: str) -> dict[str, int]:
    return {name: source[name] for name in names}


def local_read(zone: str, epoch: int) -> str:
    result = run(["date", "-r", str(epoch), "+%Y-%m-%d %H:%M:%S %Z"], env={"TZ": zone, "PATH": "/bin:/usr/bin"})
    return result.stdout.strip()


def check_transition(zone: str, when: datetime, before: tuple[int, int], after: tuple[int, int]) -> None:
    """The table's transition is the machine's: the zone reads `before` a second ahead of it and `after` at it."""
    epoch = int(when.timestamp())
    reads = [local_read(zone, epoch - 1), local_read(zone, epoch)]
    say(f"TRANSITION {when:%Y-%m-%d %H:%M:%S} UTC in {zone}: {reads[0]} -> {reads[1]}")
    before_minute, after_minute = (minutes(before) - 1) % 1440, minutes(after) % 1440
    want = [f"{before_minute // 60:02d}:{before_minute % 60:02d}:59", f"{after_minute // 60:02d}:{after_minute % 60:02d}:00"]
    if want[0] not in reads[0] or want[1] not in reads[1]:
        say(f"TRANSITION NOT AS ASSUMED: expected the local clock to read {want}")
        sys.exit(4)


def gap_entries(spec: dict) -> dict[str, dict[str, int]]:
    date, start, end, mid = spec["date"], spec["start"], spec["end"], spec["mid"]
    before, first, middle = civil(date, start, -1), civil(date, start), civil(date, mid)
    last, at_end, control = civil(date, end, -1), civil(date, end), civil(date, end, 1)
    day_first, day_end = civil(date, start), civil(date, end)
    dated = ("Month", "Day", "Hour", "Minute")
    return {
        "tB": keys(before, *dated),  # the minute before the gap
        "tS": keys(first, *dated),  # the gap's first minute
        "tM": keys(middle, *dated),
        "tL": keys(last, *dated),  # the gap's last minute
        "tE": keys(at_end, *dated),  # the gap's end, a real time
        "tF": keys(control, *dated),  # the control, one minute after the end
        "dS": keys(day_first, "Hour", "Minute"),
        "dM": keys(middle, "Hour", "Minute"),
        "dE": keys(day_end, "Hour", "Minute"),
        "yM": keys(middle, "Day", "Hour", "Minute"),
        "wM": keys(middle, "Weekday", "Hour", "Minute"),
        "hM": keys(middle, "Minute"),
        "hE": keys(at_end, "Minute"),
    }


def fold_entries(spec: dict) -> dict[str, dict[str, int]]:
    date, mid = spec["date"], spec["mid"]
    middle, control = civil(date, mid), civil(date, mid, 1)
    return {
        "s0": keys(middle, "Hour", "Minute"),
        "sd": keys(middle, "Day", "Hour", "Minute"),
        "smd": keys(middle, "Month", "Day", "Hour", "Minute"),
        "sw": keys(middle, "Weekday", "Hour", "Minute"),
        "hm": keys(middle, "Minute"),
        "c": keys(control, "Hour", "Minute"),
    }


def register(jobs: Jobs, tag: str, entries: dict[str, dict[str, int]]) -> dict[str, str]:
    """Registers one job per entry; returns name -> label."""
    labels = {name: f"goetia.probe.{tag}.{name}" for name in entries}
    for name, interval in entries.items():
        jobs.add(labels[name], interval)
    for label in labels.values():
        jobs.bootstrap(label)
    return labels


def report_registered(tag: str, entries: dict[str, dict[str, int]], labels: dict[str, str]) -> None:
    found = registered_lines(f"goetia.probe.{tag}.", list(labels.values()))
    for name, interval in entries.items():
        say(f"REGISTERED {tag} {name} {interval}: {found.get(labels[name], ['<none>'])}")


def report_fires(jobs: Jobs, zone: str, tag: str, entries: dict[str, dict[str, int]], labels: dict[str, str]) -> None:
    for name in entries:
        fires = jobs.fires(labels[name])
        shown = [f"{datetime.fromtimestamp(e, timezone.utc):%Y-%m-%d %H:%M:%S}Z = {local_read(zone, e)}" for e in fires]
        say(f"FIRES {tag} {name}: {len(fires)} {shown}")


def report_log(tag: str, start: str) -> None:
    for line in log_show(f"goetia.probe.{tag}.", start=start):
        if "Running StartCalendarInterval" in line or "Rescheduling StartCalendarInterval" in line:
            say(f"LOG {line}")


def now_local() -> str:
    return run(["date", "+%Y-%m-%d %H:%M:%S"]).stdout.strip()


def gap_rep(jobs: Jobs, clock: Clock, zone: str, spec: dict, rep: int) -> None:
    entries = gap_entries(spec)
    transition = utc(spec["U"])
    start = now_local()

    tag = f"r{rep}a"
    say(f"=== REP {rep} gap, registered 12 h before the transition")
    clock.set(transition - timedelta(hours=12))
    labels = register(jobs, tag, entries)
    report_registered(tag, entries, labels)
    for label in labels.values():
        jobs.drop(label)

    tag = f"r{rep}b"
    say(f"=== REP {rep} gap, registered {MARGIN.seconds} s before the transition")
    clock.set(transition - MARGIN)
    labels = register(jobs, tag, entries)
    say(f"registered at {datetime.now(timezone.utc):%H:%M:%S} UTC; transition at {transition:%H:%M:%S} UTC")
    report_registered(tag, entries, labels)
    wait_fires(jobs, labels["tF"], 1, FIRE_BOUND)
    report_fires(jobs, zone, tag, entries, labels)
    report_log(tag, start)
    for label in labels.values():
        jobs.drop(label)


def fold_pass_instant(spec: dict, second: bool) -> datetime:
    transition = utc(spec["U"])
    if second:
        return transition + timedelta(minutes=minutes(spec["mid"]) - minutes(spec["f0"]))
    return transition - timedelta(minutes=minutes(spec["f1"]) - minutes(spec["mid"]))


def fold_rep(jobs: Jobs, clock: Clock, zone: str, spec: dict, rep: int, kind: str) -> None:
    entries = fold_entries(spec)
    start = now_local()
    tag = f"r{rep}{kind[-1].lower()}"
    first_pass = kind == "foldA"
    say(f"=== REP {rep} {kind}, registered {MARGIN.seconds} s before the entries' time in the {'first' if first_pass else 'second'} pass")
    clock.set(fold_pass_instant(spec, second=not first_pass) - MARGIN)
    labels = register(jobs, tag, entries)
    say(f"registered at {datetime.now(timezone.utc):%H:%M:%S} UTC")
    report_registered(tag, entries, labels)
    wait_fires(jobs, labels["c"], 1, FIRE_BOUND)
    report_fires(jobs, zone, tag, entries, labels)
    if first_pass:
        say(f"=== REP {rep} {kind}: the clock jumps to {MARGIN.seconds} s before the entries' time in the second pass")
        clock.set(fold_pass_instant(spec, second=True) - MARGIN)
        wait_fires(jobs, labels["c"], 2, FIRE_BOUND)
        report_fires(jobs, zone, tag, entries, labels)
    report_log(tag, start)
    for label in labels.values():
        jobs.drop(label)


def main() -> None:
    key, kind, repetitions = sys.argv[1], sys.argv[2], int(sys.argv[3])
    zone, gap, fold = ZONES[key]
    platform_header()
    jobs, clock = Jobs(), Clock()
    install_cleanup(jobs, clock)
    use_zone(jobs, zone)
    spec = gap if kind == "gap" else fold
    if kind == "gap":
        check_transition(zone, utc(spec["U"]), spec["start"], spec["end"])
    else:
        check_transition(zone, utc(spec["U"]), spec["f1"], spec["f0"])
    for rep in range(1, repetitions + 1):
        if kind == "gap":
            gap_rep(jobs, clock, zone, spec, rep)
        else:
            fold_rep(jobs, clock, zone, spec, rep, kind)
        clock.restore()
    say(f"done at {time.strftime('%H:%M:%S')}")


if __name__ == "__main__":
    main()
