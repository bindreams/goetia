#!/usr/bin/env bash
# assert-asset-count.sh <expected-count> <asset>...
#
# Exits 0 only when exactly <expected-count> assets were named and every one
# of them exists as a file. Used to assert a release has its full asset set
# (five archives, SHA256SUMS, goetia.sigstore.json — seven) before it is
# uploaded (draft stage) or before it is trusted for publishing (publish
# stage).
#
# `nullglob` turns a missing asset into "fewer words on the command line",
# not an error, so a caller that only checks the archives
# (`assert-archives.sh`) never notices a missing SHA256SUMS or bundle, and a
# caller that uploads under `nullglob` (`gh release create source/goetia-*
# ... source/SHA256SUMS source/goetia.sigstore.json`) would silently upload
# fewer than seven assets. This script is the count check neither of those
# catches on its own.
set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "::error::usage: assert-asset-count.sh <expected-count> <asset>..." >&2
    exit 1
fi

expected_count="$1"
shift

if [[ ! "$expected_count" =~ ^[1-9][0-9]*$ ]]; then
    echo "::error::expected-count must be a positive integer, got '${expected_count}'" >&2
    exit 1
fi

if [[ "$#" -ne "$expected_count" ]]; then
    echo "::error::expected ${expected_count} asset(s), got $#: ${*:-<none>}" >&2
    exit 1
fi

for asset in "$@"; do
    if [[ ! -f "$asset" ]]; then
        echo "::error::asset not found: ${asset}" >&2
        exit 1
    fi
done

echo "all ${expected_count} asset(s) present"
