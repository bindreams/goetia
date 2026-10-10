#!/usr/bin/env bash
# Stages the root-owned binaries the launchd jobs run. Runner only.
set -euo pipefail

: "${PROBE_RUN:?}" "${PROBE_JOB:?}"
[[ "${GITHUB_ACTIONS-}" == true && "${RUNNER_OS-}" == macOS && "${RUNNER_ENVIRONMENT-}" == github-hosted ]] || {
	echo "refusing to stage: not a GitHub-hosted macOS runner" >&2
	exit 2
}

base="/tmp/goetia-probe-$PROBE_RUN"
sudo -n install -d -o root -g wheel -m 0755 "$base" "$base/bin"
sudo -n install -o root -g wheel -m 0755 probe/target/release/job probe/target/release/launchd-probe "$base/bin/"
mkdir -p "results/$PROBE_JOB"
