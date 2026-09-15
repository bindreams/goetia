#!/usr/bin/env bash
# Regression tests for run-with-timeout.py's POSIX path (`pgrep`/`pkill`/`ps`
# by session). These replace the throwaway probe script F2/F3 (commit
# 5883ce9) were verified with -- a manual check that leaves nothing behind
# for the next person to re-verify against is not a substitute for a test.
#
# Every watched command below is `sleep 2147483647` (or a wrapper that ends
# in it): a child that cannot exit on its own, so the watchdog's bound is
# provably the only way any given test can finish. Bounds passed to the
# watchdog stay short (1-2s) for the same reason -- the child can never beat
# them, so there is nothing to race.
#
# kill_tree_windows (F3) is not covered here: it needs `taskkill` and
# Windows process-tree semantics this script has no way to exercise.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
watchdog="${script_dir}/run-with-timeout.py"
failures=0

# exit-code passthrough -------------------------------------------------------

watchdog_status=0
python3 "$watchdog" 20 bash -c 'exit 7' >/dev/null 2>&1 || watchdog_status=$?
if [[ "$watchdog_status" -eq 7 ]]; then
    echo "ok   - passes through a command's own exit code unchanged"
else
    echo "FAIL - passes through a command's own exit code unchanged (expected 7, got ${watchdog_status})"
    failures=$((failures + 1))
fi

# timeout exit code ------------------------------------------------------------
#
# A single process, no descendants: isolates "the bound fired and the exit
# code says so" from the multi-process teardown the F2 test below covers.

timeout_tag="run-with-timeout-test-solo-$$-${RANDOM}"
watchdog_status=0
python3 "$watchdog" 1 bash -c "exec -a '${timeout_tag}' sleep 2147483647" >/dev/null 2>&1 || watchdog_status=$?

if [[ "$watchdog_status" -ne 124 ]]; then
    echo "FAIL - reports the script's own timeout exit code (expected 124, got ${watchdog_status})"
    failures=$((failures + 1))
elif pgrep -f "$timeout_tag" >/dev/null 2>&1; then
    echo "FAIL - reports the script's own timeout exit code (the watched process survived teardown)"
    failures=$((failures + 1))
    pkill -KILL -f "$timeout_tag" 2>/dev/null || true
else
    echo "ok   - reports the script's own timeout exit code and tears the process down"
fi

# F2 regression: a descendant moved into its own process group ---------------
#
# cosca's `.contain()` moves a contained child into a process group of its
# own, distinct from the watched root's. `killpg` (the pre-fix behavior)
# reaches only the root's own group and misses that descendant; verified
# against the pre-fix blob (git blob 1eedddec24fe70d158064f579a7bd00e1296d5f)
# during development, not re-checked here since this file's job is to pin
# the CURRENT script's behavior. `set -m` (job control) reproduces the same
# process-group split without needing cosca or Rust: a backgrounded job
# gets a process group of its own but stays in the launching root's
# session, which is exactly what `pkill -s`/`pgrep -s` key on.

f2_script="$(mktemp)"
cat > "$f2_script" <<'SCRIPT'
#!/usr/bin/env bash
set -m
exec -a "$1-child" sleep 2147483647 &
exec -a "$1-root" sleep 2147483647
SCRIPT

f2_tag="run-with-timeout-test-f2-$$-${RANDOM}"
watchdog_status=0
python3 "$watchdog" 1 bash "$f2_script" "$f2_tag" >/dev/null 2>&1 || watchdog_status=$?
rm -f "$f2_script"

if [[ "$watchdog_status" -ne 124 ]]; then
    echo "FAIL - kills a descendant moved into its own process group (expected timeout exit 124, got ${watchdog_status})"
    failures=$((failures + 1))
elif pgrep -f "$f2_tag" >/dev/null 2>&1; then
    echo "FAIL - kills a descendant moved into its own process group (a session member survived: $(pgrep -af "$f2_tag"))"
    failures=$((failures + 1))
    pkill -KILL -f "$f2_tag" 2>/dev/null || true
else
    echo "ok   - kills a descendant moved into its own process group"
fi

# zombie exclusion -------------------------------------------------------------
#
# ANY session member the kill loop SIGKILLs becomes a zombie until *its
# own* parent reaps it, not just the root pid the loop already excludes by
# identity. Relying on `pgrep -s` alone (which still lists zombies) would
# spin forever if nothing ever reaps one -- true on GitHub's runners, where
# PID 1 is systemd/launchd, but not inside a container whose PID 1 never
# reaps. Builds a real zombie -- this shell backgrounds it and has not
# `wait`-ed on it -- and asserts the script's own liveness check excludes
# it. The fixture never sleeps or polls: the parent's `cat` on the fifo
# blocks on a real kernel event (every writer closing the fifo), which only
# happens once the child holding it open has actually exited, so by the
# time `cat` returns the child is guaranteed to be a zombie already.

zombie_dir="$(mktemp -d)"
zombie_fifo="${zombie_dir}/fifo"
mkfifo "$zombie_fifo"
(exec >"$zombie_fifo"; exit 0) &
zombie_pid=$!
cat "$zombie_fifo" >/dev/null

zombie_check="$(python3 -B - "$watchdog" "$zombie_pid" <<'PY'
import importlib.util
import sys

watchdog_path, pid = sys.argv[1], int(sys.argv[2])
spec = importlib.util.spec_from_file_location("run_with_timeout", watchdog_path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
print("live" if module._is_live(pid) else "dead")
PY
)"
wait "$zombie_pid" 2>/dev/null || true
rm -rf "$zombie_dir"

if [[ "$zombie_check" == "dead" ]]; then
    echo "ok   - the liveness check excludes a real zombie"
else
    echo "FAIL - the liveness check excludes a real zombie (got '${zombie_check}', wanted 'dead')"
    failures=$((failures + 1))
fi

# --------------------------------------------------------------------------

if [[ "$failures" -gt 0 ]]; then
    echo "${failures} test(s) failed"
    exit 1
fi
echo "all tests passed"
