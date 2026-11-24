"""Usage: tz.py <norestart|restart> <repetitions>

Claim measured, on a runner that boots in GMT: after `systemsetup -settimezone America/New_York`, are calendar
entries registered BEFORE the change recomputed in the new zone, and does a restart of UserEventAgent
(`launchctl kickstart -k system/com.apple.UserEventAgent-System`) recompute them? The runner's `csrutil status` is
printed: the question asked about a real Mac with SIP on can be answered only if it says enabled.

Entry `pre` is {Hour 8, Minute 0}, registered at 12:58:30 UTC on 2026-12-01 in GMT (next fire: 08:00 GMT the next
day). Recomputed in New York (UTC-5) it would fire at 13:00 UTC, in a minute and a half. Controls registered after
the change tell when 13:00 has passed: for `norestart` {Hour 13, Minute 0} and {Hour 13, Minute 1}, which the agent
(still computing in GMT) fires at 13:00 and 13:01 UTC; for `restart` {Hour 8, Minute 0} and {Hour 8, Minute 1},
computed in New York after the restart. The last control's fire is the event after which `pre` has had its chance.

Platform: GitHub macos-latest runner, as root. Run only on a throwaway machine.
Expected output: per repetition, the registered date of `pre`, canaries, FIRES lines of `pre` and the controls.
"""

from __future__ import annotations

import sys
import time

from common import (
    Clock,
    Jobs,
    install_cleanup,
    log_show,
    platform_header,
    registered_lines,
    restart_uea,
    run,
    say,
    utc,
    wait_fires,
)

FIRE_BOUND = 200.0
NEW_YORK = "America/New_York"


def set_zone(zone: str) -> None:
    result = run(["systemsetup", "-settimezone", zone])
    say(f"systemsetup -settimezone {zone}: exit {result.returncode} {result.stdout.strip()}{result.stderr.strip()}")


def canary(jobs: Jobs, label: str) -> str:
    jobs.add(label, {"Hour": 3, "Minute": 0})
    jobs.bootstrap(label)
    line = registered_lines(label, [label]).get(label, ["<none>"])[0]
    jobs.drop(label)
    return line


def back_to_gmt(jobs: Jobs, rep: int) -> None:
    set_zone("GMT")
    restart_uea()
    line = canary(jobs, f"goetia.probe.gmt{rep}")
    say(f"CANARY [GMT, agent restarted]: {line}")
    if "at 3:00:00" not in line or "AM" not in line:
        say("the agent does not compute in GMT")
        sys.exit(3)


def until_display_follows(jobs: Jobs, rep: int) -> None:
    """Canaries until the agent's own lines name the new zone: it has noticed the change."""
    n = 0
    while True:
        n += 1
        line = canary(jobs, f"goetia.probe.disp{rep}x{n}")
        say(f"CANARY [after systemsetup, #{n}]: {line}")
        if "Eastern" in line:
            return


def rep_run(jobs: Jobs, clock: Clock, mode: str, rep: int) -> None:
    say(f"=== REP {rep} {mode}")
    back_to_gmt(jobs, rep)
    clock.set(utc("2026-12-01T12:58:30"))
    pre = f"goetia.probe.r{rep}.pre"
    jobs.add(pre, {"Hour": 8, "Minute": 0})
    jobs.bootstrap(pre)
    say(f"REGISTERED pre in GMT: {registered_lines(pre, [pre]).get(pre)}")
    set_zone(NEW_YORK)
    until_display_follows(jobs, rep)
    if mode == "restart":
        restart_uea()
        controls = {"c0": {"Hour": 8, "Minute": 0}, "c1": {"Hour": 8, "Minute": 1}}
    else:
        controls = {"c0": {"Hour": 13, "Minute": 0}, "c1": {"Hour": 13, "Minute": 1}}
    labels = {name: f"goetia.probe.r{rep}.{name}" for name in controls}
    for name, interval in controls.items():
        jobs.add(labels[name], interval)
        jobs.bootstrap(labels[name])
    found = registered_lines(f"goetia.probe.r{rep}.", list(labels.values()))
    for name in controls:
        say(f"REGISTERED control {name} {controls[name]}: {found.get(labels[name])}")
    say(f"registered at {run(['date', '-u', '+%H:%M:%S']).stdout.strip()} UTC")
    wait_fires(jobs, labels["c1"], 1, FIRE_BOUND)
    for label in [pre, *labels.values()]:
        say(f"FIRES {label}: {[time.strftime('%H:%M:%S', time.gmtime(e)) + 'Z' for e in jobs.fires(label)]}")
    for line in log_show(f"goetia.probe.r{rep}."):
        if "StartCalendarInterval" in line:
            say(f"LOG {line}")
    for label in [pre, *labels.values()]:
        jobs.drop(label)


def main() -> None:
    mode, repetitions = sys.argv[1], int(sys.argv[2])
    platform_header()
    jobs, clock = Jobs(), Clock()
    install_cleanup(jobs, clock)
    for rep in range(1, repetitions + 1):
        rep_run(jobs, clock, mode, rep)
        clock.restore()
    set_zone("GMT")
    say(f"done at {time.strftime('%H:%M:%S')}")


if __name__ == "__main__":
    main()
