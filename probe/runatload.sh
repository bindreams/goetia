#!/bin/bash
# Throwaway measurement: which plist shapes start at load, and which restart on crash / exit 1 / exit 0.
# Runs as root on a disposable GitHub-hosted macOS runner only.
set -u
[ "${GITHUB_ACTIONS:-}" = true ] && [ "${RUNNER_ENVIRONMENT:-}" = github-hosted ] && [ "${RUNNER_OS:-}" = macOS ] && [ "$(id -u)" = 0 ] \
	|| { echo "refusing: not root on a GitHub-hosted macOS runner"; exit 2; }

base="/private/var/tmp/goetia-ral.$GITHUB_RUN_ID"
mkdir -p "$base"; chmod 755 "$base"
prog="$base/prog.sh"
cat > "$prog" <<'P'
#!/bin/sh
# $1 = state dir. Log a start, then block on the command FIFO and act on what arrives.
echo "start $$" >> "$1/log"
read -r cmd < "$1/fifo"
case "$cmd" in
	crash) kill -SEGV $$ ;;
	exit1) exit 1 ;;
	exit0) exit 0 ;;
esac
exit 3
P
chmod 755 "$prog"

WINDOW=15 # seconds; a human-facing observation bound for an external process (launchd)

starts() { [ -f "$1/log" ] && wc -l < "$1/log" | tr -d ' ' || echo 0; }
# wait until the start count reaches $2, or the window passes; prints yes/no
await_starts() {
	local dir=$1 want=$2 t=0
	while [ "$(starts "$dir")" -lt "$want" ]; do
		[ "$t" -ge $((WINDOW * 5)) ] && { echo no; return; }
		sleep 0.2; t=$((t + 1))
	done
	echo yes
}
send() { echo "$2" > "$1/fifo"; } # only called when the program is known to be blocked in read
info() { launchctl print "system/$1" 2>&1 | grep -E '^\s*(state|pid|runs|last exit code|properties) =' | tr -s ' \t' ' ' | tr '\n' ';'; }

variant() {
	local name=$1 keys=$2
	local label="dev.goetia.probe.ral.$GITHUB_RUN_ID.$name"
	local dir="$base/$name"; mkdir -p "$dir"; mkfifo "$dir/fifo"; : > "$dir/log"
	local plist="/Library/LaunchDaemons/$label.plist"
	cat > "$plist" <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>$label</string>
<key>ProgramArguments</key><array><string>$prog</string><string>$dir</string></array>
<key>ThrottleInterval</key><integer>1</integer>
$keys
</dict></plist>
PL
	chown root:wheel "$plist"; chmod 644 "$plist"
	plutil -lint "$plist" >/dev/null || { echo "$name: plist invalid"; return; }
	echo
	echo "=== $name: $keys" | tr -s '\n' ' '; echo

	launchctl bootstrap system "$plist"; echo "bootstrap rc=$?"
	local at_load; at_load=$(await_starts "$dir" 1)
	echo "started at load: $at_load | $(info "$label")"
	if [ "$at_load" = no ]; then
		launchctl kickstart "system/$label"; echo "kickstart rc=$?"
		echo "started by kickstart: $(await_starts "$dir" 1)"
	fi
	local n
	for ev in crash exit1 exit0; do
		n=$(starts "$dir")
		if [ "$(launchctl print "system/$label" 2>/dev/null | grep -c 'state = running')" = 0 ]; then
			echo "$ev: not running beforehand, kickstarting"; launchctl kickstart "system/$label"; await_starts "$dir" $((n + 1)) >/dev/null; n=$(starts "$dir")
		fi
		send "$dir" "$ev"
		echo "after $ev: restarted=$(await_starts "$dir" $((n + 1))) | $(info "$label")"
	done
	launchctl bootout "system/$label" 2>/dev/null; rm -f "$plist"
}

sw_vers
variant none ''
variant ral-true '<key>RunAtLoad</key><true/>'
variant ka-true '<key>KeepAlive</key><true/>'
variant ka-true_ral-false '<key>KeepAlive</key><true/><key>RunAtLoad</key><false/>'
variant ka-succfalse '<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>'
variant ka-succfalse_ral-false '<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict><key>RunAtLoad</key><false/>'
variant ka-crashed '<key>KeepAlive</key><dict><key>Crashed</key><true/></dict>'
variant ka-crashed_ral-false '<key>KeepAlive</key><dict><key>Crashed</key><true/></dict><key>RunAtLoad</key><false/>'
variant ka-both_ral-false '<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/><key>Crashed</key><true/></dict><key>RunAtLoad</key><false/>'
variant ka-succfalse_ral-true '<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict><key>RunAtLoad</key><true/>'
