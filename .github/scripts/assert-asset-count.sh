#!/usr/bin/env bash
# assert-asset-count.sh <directory> <expected-count> <asset>...
#
# Exits 0 only when <directory> holds exactly the named assets: the right
# number of them were named, every one exists, every one is in <directory>,
# and <directory> holds nothing else. Used to assert a release has its full
# asset set (five archives, SHA256SUMS, goetia.sigstore.json — seven) before
# it is uploaded (draft stage) or before it is trusted for publishing
# (publish stage).
#
# The directory is checked, not just the argument list, because in the
# publish stage the directory IS the untrusted input: `gh release download`
# with no `--pattern` pulls down every asset on the draft, so an eighth
# asset added to a draft between the stages — an `install.sh`, a
# `goetia-setup.exe` — arrives here. Counting only the names the caller
# handed over would let it through, and publishing freezes it into an
# immutable release next to five attested archives, itself attested by
# nothing.
#
# `nullglob` turns a missing asset into "fewer words on the command line",
# not an error, so a caller that only checks the archives
# (`assert-archives.sh`) never notices a missing SHA256SUMS or bundle, and a
# caller that uploads under `nullglob` (`gh release create source/goetia-*
# ... source/SHA256SUMS source/goetia.sigstore.json`) would silently upload
# fewer than seven assets. This script is the count check neither of those
# catches on its own.
set -euo pipefail

if [[ $# -lt 3 ]]; then
    echo "::error::usage: assert-asset-count.sh <directory> <expected-count> <asset>..." >&2
    exit 1
fi

directory="$1"
expected_count="$2"
shift 2

if [[ ! "$expected_count" =~ ^[1-9][0-9]*$ ]]; then
    echo "::error::expected-count must be a positive integer, got '${expected_count}'" >&2
    exit 1
fi

if [[ "$#" -ne "$expected_count" ]]; then
    echo "::error::expected ${expected_count} asset(s), got $#: ${*:-<none>}" >&2
    exit 1
fi

if [[ ! -d "$directory" ]]; then
    echo "::error::asset directory not found: ${directory}" >&2
    exit 1
fi

for asset in "$@"; do
    if [[ ! -f "$asset" ]]; then
        echo "::error::asset not found: ${asset}" >&2
        exit 1
    fi
    if [[ "$(dirname -- "$asset")" != "$directory" ]]; then
        echo "::error::asset ${asset} is not in ${directory}; the directory check below would be checking a different directory" >&2
        exit 1
    fi
done

# Every entry, not just regular files: a stray directory is as unexpected
# here as a stray file, and reporting it is more useful than ignoring it.
mapfile -t present < <(find "$directory" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort)

if [[ "${#present[@]}" -ne "$expected_count" ]]; then
    echo "::error::${directory} holds ${#present[@]} entr(ies), expected exactly ${expected_count}: ${present[*]}" >&2
    exit 1
fi

echo "all ${expected_count} asset(s) present, and nothing else in ${directory}"
