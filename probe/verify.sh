#!/usr/bin/env bash
# After teardown: nothing carrying this run's id may remain. Reports leftovers, deletes nothing.
set -euo pipefail

: "${PROBE_RUN:?}" "${RUNNER_TEMP:?}"
[[ "${GITHUB_ACTIONS-}" == true && "${RUNNER_OS-}" == macOS && "${RUNNER_ENVIRONMENT-}" == github-hosted ]] || {
	echo "refusing to verify: not a GitHub-hosted macOS runner" >&2
	exit 2
}

run="$PROBE_RUN"
left=0
leftover() {
	echo "LEFTOVER: $*"
	left=1
}

if sudo -n launchctl print system | grep -F -- "com.goetia.probe.$run."; then leftover "launchd job"; fi
if dscl . -list /Users | grep -F -- "-$run"; then leftover "user"; fi
if dscl . -list /Groups | grep -F -- "-$run"; then leftover "group"; fi
if mount | grep -E -- "goetia-probe|goetia-test-home"; then leftover "mount"; fi
if hdiutil info | grep -F -- "goetia-probe-$run"; then leftover "attached disk image"; fi
if sudo -n ls /Library/LaunchDaemons | grep -F -- "com.goetia.probe.$run."; then leftover "plist"; fi
for p in "/tmp/goetia-probe-$run" "/private/var/goetia-probe-"*"-$run" "/private/var/db/goetia-probe-$run" \
	"/Library/Application Support/goetia-probe-$run" "/private/var/goetia-test-home-$run" "/Users/"*"-$run" \
	"/Volumes/goetia-probe-$run" "$RUNNER_TEMP/goetia-probe-$run"* "$RUNNER_TEMP/goetia-probe-"*"-$run"; do
	if [[ -e $p || -L $p ]]; then leftover "path $p"; fi
done
if [[ -e /etc/exports ]] && grep -F -- "goetia-probe" /etc/exports; then leftover "/etc/exports line"; fi

if ((left)); then
	echo "verify: leftovers remain (nothing deleted)"
	exit 1
fi
echo "verify: clean"
