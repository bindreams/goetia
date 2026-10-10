#!/usr/bin/env bash
# Runs one `launchd-probe step0` subcommand as root, from the root-owned staged binary.
# Only for a GitHub-hosted macOS runner: the binary itself refuses anywhere else.
set -euo pipefail

: "${PROBE_RUN:?}" "${PROBE_JOB:?}" "${PROBE_IMAGE:?}" "${RUNNER_TEMP:?}"

exec sudo -n /usr/bin/env \
	PATH=/usr/bin:/bin:/usr/sbin:/sbin \
	GITHUB_ACTIONS="${GITHUB_ACTIONS-}" \
	RUNNER_OS="${RUNNER_OS-}" \
	RUNNER_ENVIRONMENT="${RUNNER_ENVIRONMENT-}" \
	PROBE_RUN="$PROBE_RUN" \
	PROBE_JOB="$PROBE_JOB" \
	PROBE_IMAGE="$PROBE_IMAGE" \
	RUNNER_TEMP="$RUNNER_TEMP" \
	ImageVersion="${ImageVersion-}" \
	"/tmp/goetia-probe-$PROBE_RUN/bin/launchd-probe" step0 "$@"
