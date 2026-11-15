#!/usr/bin/env bash
# Probe (macOS CI runner, as root): StartCalendarInterval across DST in America/New_York.
# Usage: dst.sh spring|fall1|fall2
# The runner boots in GMT; UserEventAgent is restarted after `systemsetup -settimezone` (tz.sh measured that this
# makes it compute in the new zone). The clock is then set with `date -u` and each entry is a one-dict job that
# appends the epoch to a file when it runs. The next-fire times are read from UserEventAgent's `Registered` lines,
# and the fires from the files. The clock only moves forward within a case.
set -uo pipefail
exec 2>&1

if [[ "$(id -u)" -ne 0 ]]; then echo "run as root" >&2; exit 2; fi
case_name="${1:?case}"
zone=America/New_York
uea=system/com.apple.UserEventAgent-System
sw_vers; date; readlink /etc/localtime

work="$(mktemp -d /tmp/goetia-probe-dst.XXXXXX)"
labels=""
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
    echo "clock set: $(date -u) = $(date)"
}

uea_pid() { launchctl print "$uea" | awk '$1 == "pid" {print $3}'; }

write_plist() { # write_plist <label> <dict xml>; the job appends the epoch to $work/<label>.fires
    local label=$1 file="/Library/LaunchDaemons/$1.plist"
    cat > "$file" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array><string>/bin/sh</string><string>-c</string><string>date +%s >> "\$0"</string><string>$work/$label.fires</string></array>
  <key>StartCalendarInterval</key>$2
</dict></plist>
PLIST
    chown root:wheel "$file"
    chmod 644 "$file"
}

entry_xml() { # entry_xml Month=3,Day=14,Hour=2,Minute=30
    local s="<dict>" kv
    local IFS=,
    for kv in $1; do s+="<key>${kv%=*}</key><integer>${kv#*=}</integer>"; done
    echo "$s</dict>"
}

# Put UserEventAgent in the zone, and check it computes in it.
use_zone() {
    local old new label=goetia.probe.canary line=""
    systemsetup -settimezone "$zone"
    old="$(uea_pid)"
    launchctl kickstart -k "$uea"
    new="$(uea_pid)"
    until [[ -n "$new" && "$new" != "$old" ]]; do new="$(uea_pid)"; done
    write_plist "$label" "$(entry_xml Hour=3,Minute=0)"
    labels+=" $label"
    launchctl bootstrap system "/Library/LaunchDaemons/$label.plist"
    until [[ -n "$line" ]]; do
        line="$(log show --last 2m --info --debug --style compact --predicate "eventMessage CONTAINS \"$label\"" | grep "Registered StartCalendarInterval: $label" | head -1)"
    done
    echo "CANARY: $line"
    launchctl bootout "system/$label"
    case "$line" in
    # The time and "AM" are separated by U+202F, not a space.
    *"at 3:00:00"*AM*Eastern*) echo "zone ok: UserEventAgent computes in $zone" ;;
    *) echo "zone NOT applied"; exit 3 ;;
    esac
}

use_zone
stream="$work/stream.log"
log stream --level debug --style compact --predicate 'eventMessage CONTAINS "StartCalendarInterval"' > "$stream" 2>&1 &
stream_pid=$!
until [[ -s "$stream" ]]; do sleep 0.1; done
logger "StartCalendarInterval canary"
until grep -q canary "$stream"; do sleep 0.1; done

reg_line() { # reg_line <name>: the Registered line, from the stream or else the log
    local label="goetia.probe.$1" line=""
    until [[ -n "$line" ]]; do
        line="$(grep "Registered StartCalendarInterval: $label\." "$stream" | head -1)"
        if [[ -z "$line" ]]; then
            line="$(log show --last 5m --info --debug --style compact --predicate "eventMessage CONTAINS \"$label\"" | grep "Registered StartCalendarInterval: $label\." | head -1)"
        fi
    done
    echo "REGISTERED $1: $line"
}

step_names=""
step() { # step <title> <clock> <name:Key=V,Key=V>...
    local title=$1 clock=$2 spec name
    shift 2
    echo "=== STEP $title"
    set_clock "$clock"
    step_names=""
    for spec in "$@"; do
        name=${spec%%:*}
        write_plist "goetia.probe.$name" "$(entry_xml "${spec#*:}")"
        labels+=" goetia.probe.$name"
        step_names+=" $name"
    done
    for name in $step_names; do launchctl bootstrap system "/Library/LaunchDaemons/goetia.probe.$name.plist"; done
    echo "bootstrapped at $(date -u) = $(date)"
    for name in $step_names; do reg_line "$name"; done
}

wait_fires() { # wait_fires <name> <count> <bound in seconds>: a failure bound for an event that may never come
    local f="$work/goetia.probe.$1.fires" start
    start="$(now_mono)"
    until [[ -f "$f" && "$(wc -l < "$f" | tr -d ' ')" -ge "$2" ]]; do
        if python3 -c "import sys,time; sys.exit(0 if time.monotonic() - $start > $3 else 1)"; then
            echo "NO FIRE: $1 did not reach $2 fire(s) in $3 s"
            return 1
        fi
        sleep 0.5
    done
}

show() { # the fires and the log lines of the step's jobs
    local name e
    for name in $step_names; do
        echo "FIRES $name:"
        if [[ -f "$work/goetia.probe.$name.fires" ]]; then
            while IFS= read -r e; do echo "  $e  UTC $(date -u -r "$e" '+%Y-%m-%d %H:%M:%S')  local $(date -r "$e" '+%Y-%m-%d %H:%M:%S %Z')"; done < "$work/goetia.probe.$name.fires"
        else
            echo "  none"
        fi
        grep -E "(Running|Rescheduling) StartCalendarInterval: goetia.probe.$name\." "$stream"
        echo "  $(launchctl print "system/goetia.probe.$name" | grep -E 'runs =')"
    done
}

unload() {
    local name
    for name in $step_names; do
        launchctl bootout "system/goetia.probe.$name"
        rm -f "/Library/LaunchDaemons/goetia.probe.$name.plist"
    done
}

# Spring forward: 2027-03-14, 02:00 EST -> 03:00 EDT at 07:00 UTC.
# Fall back: 2027-11-07, 02:00 EDT -> 01:00 EST at 06:00 UTC; 01:00 to 02:00 local happens twice.
case "$case_name" in
spring)
    step "spring A: the day before (12:00 EST), daily and dated entries" 031317002027.00 \
        d159:Hour=1,Minute=59 d200:Hour=2,Minute=0 d230:Hour=2,Minute=30 d300:Hour=3,Minute=0 d330:Hour=3,Minute=30 \
        t230:Month=3,Day=14,Hour=2,Minute=30 t330:Month=3,Day=14,Hour=3,Minute=30
    unload
    step "spring B: 01:57:30 EST, entries around the gap" 031406572027.30 \
        t159:Month=3,Day=14,Hour=1,Minute=59 t200:Month=3,Day=14,Hour=2,Minute=0 t230:Month=3,Day=14,Hour=2,Minute=30 \
        t300:Month=3,Day=14,Hour=3,Minute=0 t301:Month=3,Day=14,Hour=3,Minute=1 t302:Month=3,Day=14,Hour=3,Minute=2 \
        t330:Month=3,Day=14,Hour=3,Minute=30 t332:Month=3,Day=14,Hour=3,Minute=32 \
        d230:Hour=2,Minute=30
    wait_fires t302 1 900
    show
    echo "=== STEP spring C: the clock jumps to 03:29:40 EDT (07:29:40 UTC), after the entries were registered"
    set_clock 031407292027.40
    wait_fires t332 1 400
    show
    unload
    ;;
fall1)
    step "fall1 B1: 01:29:40 EDT, first pass of the fold" 110705292027.40 \
        d130:Hour=1,Minute=30 t130:Month=11,Day=7,Hour=1,Minute=30 t131:Month=11,Day=7,Hour=1,Minute=31
    wait_fires t131 1 300
    show
    echo "=== STEP fall1 C: the clock jumps to 01:29:40 EST (06:29:40 UTC), second pass, after the entries were registered"
    set_clock 110706292027.40
    wait_fires d130 2 300
    wait_fires t130 2 5
    show
    unload
    ;;
fall2)
    step "fall2 A: the day before (13:00 EDT), daily and dated entries" 110617002027.00 \
        d059:Hour=0,Minute=59 d130:Hour=1,Minute=30 d159:Hour=1,Minute=59 d200:Hour=2,Minute=0 d201:Hour=2,Minute=1 \
        t130:Month=11,Day=7,Hour=1,Minute=30
    unload
    step "fall2 B3: 01:57:30 EDT, entries around the fold" 110705572027.30 \
        d159:Hour=1,Minute=59 t059:Month=11,Day=7,Hour=0,Minute=59 t130:Month=11,Day=7,Hour=1,Minute=30 \
        t159:Month=11,Day=7,Hour=1,Minute=59 t200:Month=11,Day=7,Hour=2,Minute=0 t201:Month=11,Day=7,Hour=2,Minute=1
    wait_fires d159 1 300
    show
    unload
    step "fall2 B2: 01:29:40 EST (06:29:40 UTC), registered in the second pass" 110706292027.40 \
        t130:Month=11,Day=7,Hour=1,Minute=30 t131:Month=11,Day=7,Hour=1,Minute=31
    wait_fires t131 1 300
    show
    unload
    ;;
*)
    echo "unknown case $case_name" >&2
    exit 2
    ;;
esac
date
