#!/usr/bin/env bash
# Usage: launchd-calendar-dst.sh spring_a|spring_b|spring_c|fall1|fall2_a|fall2_b|fall3a|fall3b|hourly_spring_a|hourly_spring_b|hourly_fall_a|hourly_fall_b
# Claim: StartCalendarInterval across DST in America/New_York. An entry in a spring-forward gap is registered for and
# fires once at 03:00:00 EDT (the end of the gap); in a fall-back fold, an entry with no date or a Weekday fires at both
# passes and an entry with a Day or Month and Day only at the first. Read the REGISTERED, FIRES and Rescheduling lines.
# Platform: GitHub macos-latest runner (macos-26-arm64 20261102.0473.1, macOS 26.7.2), as root. Run only on a throwaway
# machine. Keep each case to about two minutes with the clock in 2027, and send the output to a file printed after the
# clock is back: longer runs lost their log.
# Expected output, spring_b: t200, t230 and d230 REGISTERED for 3:00:00 AM EDT, and one fire each at 07:00:04 UTC.
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
spring_a)
    step "spring A: the day before (12:00 EST), daily and dated entries" 031317002027.00         d159:Hour=1,Minute=59 d200:Hour=2,Minute=0 d230:Hour=2,Minute=30 d300:Hour=3,Minute=0 d330:Hour=3,Minute=30         t230:Month=3,Day=14,Hour=2,Minute=30 t330:Month=3,Day=14,Hour=3,Minute=30
    unload
    ;;
spring_b)
    step "spring B0: 01:58:30 EST, an entry before the gap" 031406582027.30 t159:Month=3,Day=14,Hour=1,Minute=59
    wait_fires t159 1 120
    show
    unload
    step "spring B2: 01:59:40 EST, entries in and after the gap" 031406592027.40         t200:Month=3,Day=14,Hour=2,Minute=0 t230:Month=3,Day=14,Hour=2,Minute=30         t300:Month=3,Day=14,Hour=3,Minute=0 t301:Month=3,Day=14,Hour=3,Minute=1 d230:Hour=2,Minute=30
    wait_fires t301 1 150
    show
    unload
    ;;
spring_c)
    step "spring C0: 01:59:40 EST, entries registered before the transition" 031406592027.40         t230:Month=3,Day=14,Hour=2,Minute=30 t330:Month=3,Day=14,Hour=3,Minute=30 c331:Month=3,Day=14,Hour=3,Minute=31         d230:Hour=2,Minute=30
    echo "=== STEP spring C: the clock jumps to 03:29:40 EDT (07:29:40 UTC), after the entries were registered"
    set_clock 031407292027.40
    wait_fires c331 1 150
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
fall3a)
    # The four shapes goetia's compiler emits (no date, Day, Month and Day, Weekday) at 01:30, in the fold. 2027-11-07 is a Sunday.
    # The daily control c31 fires at 01:31 in the first pass; the Rescheduling lines show whether a second pass follows.
    step "fall3a: 01:29:40 EDT, first pass of the fold" 110705292027.40 \
        s0:Hour=1,Minute=30 sd:Day=7,Hour=1,Minute=30 smd:Month=11,Day=7,Hour=1,Minute=30 sw:Weekday=0,Hour=1,Minute=30 \
        c31:Hour=1,Minute=31
    wait_fires c31 1 150
    show
    unload
    ;;
fall3b)
    step "fall3b: 01:29:40 EST (06:29:40 UTC), second pass of the fold" 110706292027.40 \
        s0:Hour=1,Minute=30 sd:Day=7,Hour=1,Minute=30 smd:Month=11,Day=7,Hour=1,Minute=30 sw:Weekday=0,Hour=1,Minute=30 \
        c31:Hour=1,Minute=31
    wait_fires c31 1 150
    show
    unload
    ;;
fall2_a)
    step "fall2 A: the day before (13:00 EDT), daily and dated entries" 110617002027.00         d059:Hour=0,Minute=59 d130:Hour=1,Minute=30 d159:Hour=1,Minute=59 d200:Hour=2,Minute=0 d201:Hour=2,Minute=1         t130:Month=11,Day=7,Hour=1,Minute=30
    unload
    ;;
fall2_b)
    step "fall2 B3: 01:57:30 EDT, entries around the fold" 110705572027.30         d159:Hour=1,Minute=59 t059:Month=11,Day=7,Hour=0,Minute=59 t130:Month=11,Day=7,Hour=1,Minute=30         t159:Month=11,Day=7,Hour=1,Minute=59 t200:Month=11,Day=7,Hour=2,Minute=0 t201:Month=11,Day=7,Hour=2,Minute=1
    wait_fires d159 1 150
    show
    unload
    ;;
hourly_spring_a)
    # An hourly entry {Minute 30} (cron `30 * * * *`) across the gap. 2027-03-14 01:30 EST is 06:30 UTC.
    step "hourly spring A: 01:29:40 EST, the fire before the gap" 031406292027.40 h1:Minute=30
    wait_fires h1 1 120
    show
    unload
    ;;
hourly_spring_b)
    step "hourly spring B: 01:59:40 EST, 20 s before the gap's end" 031406592027.40 h2:Minute=30
    wait_fires h2 1 90
    show
    unload
    step "hourly spring C: 03:10 EDT, after the gap" 031407102027.00 h3:Minute=30
    unload
    ;;
hourly_fall_a)
    # The fold: 01:30 EDT is 05:30 UTC and 01:30 EST is 06:30 UTC; the clocks go back at 06:00 UTC.
    step "hourly fall A: 01:29:40 EDT, the first pass" 110705292027.40 h1:Minute=30
    wait_fires h1 1 120
    show
    unload
    step "hourly fall B: 01:40 EDT, registered after the first pass's 01:30" 110705402027.00 h2:Minute=30
    unload
    ;;
hourly_fall_b)
    step "hourly fall C: 01:29:40 EST (06:29:40 UTC), the second pass" 110706292027.40 h3:Minute=30
    wait_fires h3 1 120
    show
    unload
    step "hourly fall D: 01:40 EST, registered after the second pass's 01:30" 110706402027.00 h4:Minute=30
    unload
    ;;
*)
    echo "unknown case $case_name" >&2
    exit 2
    ;;
esac
date
