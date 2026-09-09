#!/usr/bin/env bash
# assert-test-binaries.sh <listing-file>
#
# Exits 0 only when <listing-file> names at least one test binary. CI's "Run
# tests, elevated" step drives its `while read` loop from this file; an
# empty file makes the loop iterate zero times and the step exits 0 having
# run nothing — the exact failure mode this guards against, and on this
# branch that step is the release gate. It can happen if
# `cargo test --message-format=json`'s schema changes and the Python filter
# that produces the listing stops matching anything.
#
# Counting is done the same way the workflow's own loop does — strip a
# trailing CR (Windows' text-mode stdout writes CRLF) and skip blank lines —
# so a listing that is technically non-empty in bytes but contains only
# blank/CR lines is still correctly counted as zero.
set -euo pipefail

listing="${1:?listing file required}"

if [[ ! -f "$listing" ]]; then
    echo "::error::test binaries listing not found: ${listing}" >&2
    exit 1
fi

count=0
while IFS= read -r bin; do
    bin="${bin%$'\r'}"
    if [[ -z "$bin" ]]; then
        continue
    fi
    count=$((count + 1))
done < "$listing"

if [[ "$count" -eq 0 ]]; then
    echo "::error::no test binaries found in ${listing}; the test gate would run zero tests and report success. Check for a 'cargo test --message-format=json' schema change." >&2
    exit 1
fi

echo "found ${count} test binary(ies) to run"
