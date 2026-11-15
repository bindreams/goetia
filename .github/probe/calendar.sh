#!/usr/bin/env bash
# Probe (macOS CI runner, as root, zone GMT): what launchd registers for a calendar, and whether it matches goetia's next-run.
# Usage: calendar.sh <path to the calendar_probe example>
# Part A: does `launchctl print system/<label>` show the registered entries (plain plist, array of dicts)?
# Part B: for a schedule installed through goetia, the earliest `Registered` line of the timer's entries (launchd's next
# fire) against goetia's `next_run` (from `status`), at several clock readings. The clock only moves forward.
set -uo pipefail
exec 2>&1

if [[ "$(id -u)" -ne 0 ]]; then echo "run as root" >&2; exit 2; fi
probe="$(cd "$(dirname "${1:?path to calendar_probe}")" && pwd)/$(basename "$1")"
here="$(cd "$(dirname "$0")" && pwd)"
sw_vers; date; readlink /etc/localtime

work="$(mktemp -d /tmp/goetia-probe-cal.XXXXXX)"
labels=""
ids=""
stream_pid=""
clock_changed=""
epoch0="$(date +%s)"
now_mono() { python3 -c 'import time; print(time.monotonic())'; }
mono0="$(now_mono)"

restore_clock() {
    local target
    if [[ -n "$clock_changed" ]]; then
        target="$(python3 -c "print(int($epoch0 + $(now_mono) - $mono0))")"
        date -u "$(date -u -r "$target" +%m%d%H%M%Y.%S)"
        systemsetup -setusingnetworktime on
        date
    fi
}
cleanup() {
    local l
    if [[ -n "$stream_pid" ]]; then kill "$stream_pid"; fi
    for l in $ids; do "$probe" uninstall "$l"; done
    for l in $labels; do
        launchctl bootout "system/$l"
        rm -f "/Library/LaunchDaemons/$l.plist"
    done
    rm -rf "$work"
    restore_clock
}
trap cleanup EXIT

set_clock() { # set_clock <mmddHHMMccyy.SS>, UTC
    systemsetup -setusingnetworktime off
    clock_changed=1
    date -u "$1"
    echo "clock set: $(date -u)"
}

stream="$work/stream.log"
log stream --level debug --style compact --predicate 'eventMessage CONTAINS "StartCalendarInterval"' > "$stream" 2>&1 &
stream_pid=$!
until [[ -s "$stream" ]]; do sleep 0.1; done
logger "StartCalendarInterval canary"
until grep -q canary "$stream"; do sleep 0.1; done

# The Registered lines of a label, from the stream or else the log, once there are $2 of them (distinct entries).
reg_lines() { # reg_lines <label> <count>
    local label=$1 count=$2 lines=""
    until [[ -n "$lines" && "$(echo "$lines" | wc -l | tr -d ' ')" -ge "$count" ]]; do
        lines="$(grep -F "Registered StartCalendarInterval: $label." "$stream" | sort -u)"
        if [[ -z "$lines" || "$(echo "$lines" | wc -l | tr -d ' ')" -lt "$count" ]]; then
            lines="$(log show --last 5m --info --debug --style compact --predicate "eventMessage CONTAINS \"$label\"" | grep -F "Registered StartCalendarInterval: $label." | sort -u)"
        fi
    done
    echo "$lines"
}

echo "=== PART A: does print show the registered entries?"
label=goetia.probe.printarray
labels+=" $label"
cat > "/Library/LaunchDaemons/$label.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array><string>/usr/bin/true</string></array>
  <key>StartCalendarInterval</key><array>
    <dict><key>Hour</key><integer>3</integer><key>Minute</key><integer>0</integer></dict>
    <dict><key>Hour</key><integer>4</integer><key>Minute</key><integer>30</integer></dict>
    <dict><key>Weekday</key><integer>1</integer><key>Hour</key><integer>5</integer><key>Minute</key><integer>15</integer></dict>
  </array>
</dict></plist>
PLIST
chown root:wheel "/Library/LaunchDaemons/$label.plist"
launchctl bootstrap system "/Library/LaunchDaemons/$label.plist"
echo "REGISTERED lines of $label:"
reg_lines "$label" 3
echo "--- launchctl print system/$label"
launchctl print "system/$label"
echo "--- launchctl print system/$label, lines mentioning an entry or a time"
launchctl print "system/$label" | grep -nE 'Hour|Minute|Weekday|Day|Month|descriptor|calendar|next|fire|[0-9]{4}'
echo "--- launchctl dumpstate, lines of the job that mention an entry or a time"
launchctl dumpstate | grep -n "$label"
launchctl bootout "system/$label"

echo "=== PART B: goetia's compiler and next-run against launchd's registered next fires"
idx=0
compare() { # compare <cron> <clock>...
    local cron=$1 clock id timer n out goetia launchd lines
    shift
    for clock in "$@"; do
        idx=$((idx + 1))
        id="calprobe$idx"
        ids+=" $id"
        set_clock "$clock"
        out="$("$probe" install "$id" "$cron" /bin/sh -c true)"
        echo "$out"
        goetia="$(echo "$out" | awk '$1 == "GOETIA_NEXT_RUN" {print $2}')"
        timer="$id@timer"
        n="$(plutil -extract StartCalendarInterval raw -o - "/Library/LaunchDaemons/$timer.plist")"
        lines="$(reg_lines "$timer" "$n")"
        launchd="$(echo "$lines" | python3 -I "$here/reg.py" | sort -n | head -1)"
        if [[ "$idx" -eq 1 ]]; then
            echo "--- launchctl print system/$timer (first step only)"
            launchctl print "system/$timer" | head -80
            echo "--- print line count: $(launchctl print "system/$timer" | wc -l | tr -d ' ')"
        fi
        echo "COMPARE [$cron] now=$(date -u '+%Y-%m-%d %H:%M:%S') entries=$n registered_lines=$(echo "$lines" | wc -l | tr -d ' ') launchd_next=$launchd ($(date -u -r "$launchd" '+%Y-%m-%d %H:%M:%S' 2>/dev/null)) goetia_next=$goetia ($(date -u -r "$goetia" '+%Y-%m-%d %H:%M:%S' 2>/dev/null)) $([[ "$launchd" == "$goetia" ]] && echo MATCH || echo DIFFER)"
        "$probe" uninstall "$id"
    done
}

compare '*/15 9-17 * * 1-5' 010408502027.10 010409072027.20 010817462027.00 010912002027.00 011023592027.30
compare '0 3 1,15 * *' 011412002027.00 011503052027.00 013123502027.00 021412002027.00 121504002027.00
date
