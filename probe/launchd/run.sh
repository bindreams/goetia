#!/bin/bash
# What `launchctl bootout` reports when (a) the job ignores SIGTERM and (b)
# the job's descendant has left its process group (setsid). ExitTimeOut=5.
set -u
run_case() {
  local label=$1 program=$2
  local plist=/Library/LaunchDaemons/$label.plist
  cat > /tmp/$label.plist <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>$label</string>
<key>ProgramArguments</key><array><string>/bin/bash</string><string>-c</string><string>$program</string></array>
<key>RunAtLoad</key><true/>
<key>ExitTimeOut</key><integer>5</integer>
</dict></plist>
PLIST
  rm -f /tmp/$label.ready; mkfifo -m 666 /tmp/$label.ready
  sudo cp /tmp/$label.plist "$plist"
  sudo launchctl bootstrap system "$plist"
  read -r _ < /tmp/$label.ready   # blocks until the job writes; the job timeout bounds it
  echo "=== $label"
  echo "  before: $(sudo launchctl print system/$label 2>&1 | grep -E '^\s*(state|pid) =' | tr -s ' ' | tr '\n' ';')"
  local start=$(date +%s)
  out=$(sudo launchctl bootout system/$label 2>&1); rc=$?
  echo "  bootout rc=$rc after $(( $(date +%s) - start ))s out=[$out]"
  echo "  print after: $(sudo launchctl print system/$label 2>&1 | head -1)"
  echo "  survivors: [$(pgrep -fl "marker-$label" | tr '\n' ';')]"
  pkill -9 -f "marker-$label" || true
  sudo rm -f "$plist"
}
run_case probe.termignore 'trap "" TERM; echo ready > /tmp/probe.termignore.ready; exec -a marker-probe.termignore bash -c "trap \"\" TERM; while :; do sleep 1; done"'
run_case probe.setsid 'perl -MPOSIX -e "fork and exit; POSIX::setsid(); \$0=q(marker-probe.setsid); sleep 100000" & echo ready > /tmp/probe.setsid.ready; while :; do sleep 1; done'
