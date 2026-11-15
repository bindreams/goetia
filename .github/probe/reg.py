"""Reads UserEventAgent `Registered StartCalendarInterval: <label>.<n>: <date>` lines on stdin and prints the epoch of
each date, one per line. The zone name must be one this probe understands (GMT)."""

import re
import sys
from datetime import datetime, timedelta, timezone

ZONES = {
    "Greenwich Mean Time": timezone.utc,
    "Eastern Standard Time": timezone(timedelta(hours=-5)),
    "Eastern Daylight Time": timezone(timedelta(hours=-4)),
}
LINE = re.compile(r": \w+, (\w+) (\d+), (\d+) at (\d+):(\d+):(\d+) (AM|PM) (.+)$")

for raw in sys.stdin:
    text = raw.replace(" ", " ").strip()
    m = LINE.search(text)
    if not m:
        sys.exit(f"cannot read: {text!r}")
    month, day, year, hour, minute, second, half, zone = m.groups()
    when = datetime.strptime(f"{month} {day} {year} {hour}:{minute}:{second} {half}", "%B %d %Y %I:%M:%S %p")
    print(int(when.replace(tzinfo=ZONES[zone]).timestamp()))
