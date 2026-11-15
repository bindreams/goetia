#!/usr/bin/env bash
# Probe (macOS CI runner, as root): can UserEventAgent be made to compute StartCalendarInterval in a zone with DST,
# after the machine booted in GMT, short of a reboot?
# Usage: tz.sh <method>
# After each step a canary {Hour 3, Minute 0} is registered and its `Registered` line read: "3:00:00 AM" in it means the
# entry was computed in America/New_York; "10:00:00 PM" (or any other hour) means in GMT.
set -uo pipefail
exec 2>&1

method="${1:?method}"
zone=America/New_York
uea=system/com.apple.UserEventAgent-System
n=0
labels=""

echo "=== platform"
sw_vers; uname -a; echo "ImageOS=${ImageOS:-} ImageVersion=${ImageVersion:-}"; csrutil status || true
date; readlink /etc/localtime; systemsetup -gettimezone || true

cleanup() {
    local l
    for l in $labels; do
        launchctl bootout "system/$l" || true
        rm -f "/Library/LaunchDaemons/$l.plist"
    done
}
trap cleanup EXIT

uea_pid() { launchctl print "$uea" | awk '$1 == "pid" {print $3}'; }

canary() { # canary <what>: registers a fresh {Hour 3} job, prints its Registered line and a verdict
    local what=$1 label line
    n=$((n + 1))
    label="goetia.probe.canary$n"
    labels+=" $label"
    cat > "/Library/LaunchDaemons/$label.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array><string>/usr/bin/true</string></array>
  <key>StartCalendarInterval</key><dict><key>Hour</key><integer>3</integer><key>Minute</key><integer>0</integer></dict>
</dict></plist>
PLIST
    chown root:wheel "/Library/LaunchDaemons/$label.plist"
    launchctl bootstrap system "/Library/LaunchDaemons/$label.plist"
    line=""
    until [[ -n "$line" ]]; do
        line="$(log show --last 2m --info --debug --style compact --predicate "eventMessage CONTAINS \"$label\"" | grep "Registered StartCalendarInterval: $label" | head -1)"
    done
    echo "CANARY [$what] $(date '+%H:%M:%S %Z'): $line"
    case "$line" in
    *" 3:00:00 AM "*) echo "VERDICT [$what]: computed in $zone" ;;
    *) echo "VERDICT [$what]: NOT computed in $zone" ;;
    esac
    launchctl bootout "system/$label"
}

set_zone() {
    systemsetup -settimezone "$zone"
    echo "after settimezone: $(readlink /etc/localtime) | $(systemsetup -gettimezone)"
}

link_zone() {
    ln -sf "/var/db/timezone/zoneinfo/$zone" /etc/localtime
    echo "after relink: $(readlink /etc/localtime) | $(systemsetup -gettimezone) | date: $(date)"
}

# Wait until UEA's own log lines carry the new zone's name: it has noticed the change (not that it computes in it).
display_follows() {
    local label line
    until false; do
        n=$((n + 1))
        label="goetia.probe.canary$n"
        labels+=" $label"
        cat > "/Library/LaunchDaemons/$label.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array><string>/usr/bin/true</string></array>
  <key>StartCalendarInterval</key><dict><key>Hour</key><integer>3</integer></dict>
</dict></plist>
PLIST
        launchctl bootstrap system "/Library/LaunchDaemons/$label.plist"
        line=""
        until [[ -n "$line" ]]; do
            line="$(log show --last 2m --info --debug --style compact --predicate "eventMessage CONTAINS \"$label\"" | grep "Registered StartCalendarInterval: $label" | head -1)"
        done
        launchctl bootout "system/$label"
        echo "display probe $(date '+%H:%M:%S'): $line"
        case "$line" in *Eastern*) return ;; esac
    done
}

uea_restart_kick() {
    local old new
    old="$(uea_pid)"
    echo "UEA pid before: $old"
    launchctl kickstart -k "$uea"; echo "kickstart -k exit $?"
    new="$(uea_pid)"
    until [[ -n "$new" && "$new" != "$old" ]]; do new="$(uea_pid)"; done
    echo "UEA pid after: $new"
}

uea_restart_kill() {
    local old new
    old="$(uea_pid)"
    echo "UEA pid before: $old"
    kill -9 "$old"; echo "kill -9 exit $?"
    new="$(uea_pid)"
    until [[ -n "$new" && "$new" != "$old" ]]; do new="$(uea_pid)"; done
    echo "UEA pid after: $new"
}

uea_restart_bootout() {
    launchctl bootout "$uea"; echo "bootout exit $?"
    launchctl bootstrap system /System/Library/LaunchDaemons/com.apple.UserEventAgent-System.plist; echo "bootstrap exit $?"
    echo "UEA pid after: $(uea_pid)"
}

echo "=== method $method"
canary "baseline, GMT boot"
case "$method" in
ss)
    set_zone
    display_follows
    canary "after systemsetup, display follows"
    ;;
ss-kick)
    set_zone
    display_follows
    canary "after systemsetup, display follows"
    uea_restart_kick
    canary "after kickstart -k of UserEventAgent"
    ;;
ss-kill)
    set_zone
    display_follows
    canary "after systemsetup, display follows"
    uea_restart_kill
    canary "after kill -9 of UserEventAgent"
    ;;
ss-bootout)
    set_zone
    display_follows
    canary "after systemsetup, display follows"
    uea_restart_bootout
    canary "after bootout and bootstrap of UserEventAgent"
    ;;
link-kick)
    link_zone
    canary "after relink of /etc/localtime"
    uea_restart_kick
    canary "after kickstart -k of UserEventAgent"
    ;;
ss-setenv-kick)
    set_zone
    display_follows
    launchctl setenv TZ "$zone"; echo "setenv exit $?; getenv: $(launchctl getenv TZ)"
    uea_restart_kick
    canary "after systemsetup, setenv TZ and kickstart -k of UserEventAgent"
    ;;
setenv-kick)
    launchctl setenv TZ "$zone"; echo "setenv exit $?; getenv: $(launchctl getenv TZ)"
    uea_restart_kick
    canary "after setenv TZ alone and kickstart -k of UserEventAgent"
    ;;
ss-notify)
    set_zone
    display_follows
    notifyutil -p com.apple.system.timezone; echo "notifyutil exit $?"
    canary "after notifyutil -p com.apple.system.timezone"
    ;;
*)
    echo "unknown method $method" >&2
    exit 2
    ;;
esac

echo "=== final state"
date; readlink /etc/localtime; systemsetup -gettimezone; launchctl print "$uea" | head -40
