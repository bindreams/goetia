#!/usr/bin/env bash
# Tests for run-with-timeout.py on POSIX. The two cases here run the whole
# script under a real bound and check its exit-code wiring, where a bound that
# fires early cannot hide a defect. The kill path is tested against real
# process trees by run-with-timeout-posix-tests.py, run last.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
watchdog="${script_dir}/run-with-timeout.py"
failures=0

# exit-code passthrough -------------------------------------------------------

watchdog_status=0
python3 "$watchdog" 20 bash -c 'exit 7' || watchdog_status=$?
if [[ "$watchdog_status" -eq 7 ]]; then
    echo "ok   - passes through a command's own exit code unchanged"
else
    echo "FAIL - passes through a command's own exit code unchanged (expected 7, got ${watchdog_status})"
    failures=$((failures + 1))
fi

# timeout exit code ------------------------------------------------------------
#
# A single process that cannot exit on its own, so the bound is what ends it.
# The tag is in the command line from the start, so the survivor check below
# finds the root whether the bound fired before or after bash exec'd `sleep`.

timeout_tag="run-with-timeout-test-solo-$$-${RANDOM}"
watchdog_status=0
python3 "$watchdog" 1 bash -c "exec -a '${timeout_tag}' sleep 2147483647" || watchdog_status=$?

if [[ "$watchdog_status" -ne 124 ]]; then
    echo "FAIL - reports the script's own timeout exit code (expected 124, got ${watchdog_status})"
    failures=$((failures + 1))
elif pgrep -f "$timeout_tag" >/dev/null; then
    echo "FAIL - reports the script's own timeout exit code (the watched process survived teardown)"
    failures=$((failures + 1))
    pkill -KILL -f "$timeout_tag" || true
else
    echo "ok   - reports the script's own timeout exit code and tears the process down"
fi

# kill path, against real process trees ----------------------------------------

if ! python3 -B "${script_dir}/run-with-timeout-posix-tests.py"; then
    echo "FAIL - run-with-timeout-posix-tests.py"
    failures=$((failures + 1))
fi

# --------------------------------------------------------------------------

if [[ "$failures" -gt 0 ]]; then
    echo "${failures} test(s) failed"
    exit 1
fi
echo "all tests passed"
