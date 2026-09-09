#!/usr/bin/env bash
# assert-package-list.sh <listing-file>
# assert-package-list.sh --print-excluded
#
# Exits 0 only when a `cargo package --list` listing (one path per line, as
# produced by the `package` job's own step) both ships what a consumer
# needs and excludes what Cargo.toml's `exclude` says it should.
#
# Presence is checked first, and is the point of the positive control: a
# `grep -E` for excluded prefixes that finds nothing exits 1 identically
# whether nothing leaked or the listing itself is empty or truncated — so
# before trusting a clean exclusion result, this asserts that a path known
# to ship (`Cargo.toml`, `src/lib.rs`) and both `[[bin]]` sources
# (`src/main.rs`, `src/bin/shim/main.rs`) are actually present. The bin
# sources double as a build-breaking check of their own: an `exclude` entry
# that swallowed one would fail every consumer's build, not just fail to
# publish.
#
# The exclusion check then follows the same three-way branch as
# assert-tag-absent.sh: 0 means a path leaked (name the offenders and
# fail), 1 means clean, anything else means grep itself errored and the
# exclusion is unverified (also fail) — a failed check and a passing check
# are different outcomes.
#
# `--print-excluded` prints the exclusion list, one entry per line, so the
# test suite can compare it against Cargo.toml's `exclude` and fail on
# drift. It has to be checked that way rather than derived here: `cargo
# metadata` does not expose `exclude` at all (its package object carries
# name, version, targets, features and so on, but no include/exclude), and
# this script runs on all three CI hosts, where a TOML parser is not
# something to depend on.
set -euo pipefail

# goetia's exclude list (Cargo.toml's `exclude`, with the leading `/` that
# anchors each entry to the package root dropped). A trailing `/` marks a
# directory prefix, which matches any path under it; every other entry is a
# single file, anchored at both ends.
excluded_paths=(
    ".github/"
    ".claude/"
    "scripts/"
    "prek.toml"
    ".editorconfig"
    ".gitattributes"
    ".rustfmt.toml"
)

if [[ "${1-}" == "--print-excluded" ]]; then
    printf '%s\n' "${excluded_paths[@]}"
    exit 0
fi

listing="${1:?listing file required}"

if [[ ! -f "$listing" ]]; then
    echo "::error::listing file not found: ${listing}" >&2
    exit 1
fi

required_paths=(
    "Cargo.toml"
    "src/lib.rs"
    "src/main.rs"
    "src/bin/shim/main.rs"
)

for required in "${required_paths[@]}"; do
    if ! grep -qxF -- "$required" "$listing"; then
        echo "::error::${required} is missing from the package listing; either it was wrongly excluded, or the listing itself is empty/truncated and cannot be trusted" >&2
        exit 1
    fi
done

alternatives=()
for excluded in "${excluded_paths[@]}"; do
    escaped="${excluded//./\\.}"
    if [[ "$excluded" == */ ]]; then
        alternatives+=("$escaped")
    else
        alternatives+=("${escaped}\$")
    fi
done
excluded_pattern="^($(IFS='|'; printf '%s' "${alternatives[*]}"))"

status=0
grep -E "$excluded_pattern" "$listing" >/dev/null 2>&1 || status=$?

case "$status" in
    0)
        echo "::error::excluded paths leaked into the package:" >&2
        grep -E "$excluded_pattern" "$listing" >&2
        exit 1
        ;;
    1)
        echo "no excluded paths in the package"
        ;;
    *)
        echo "::error::grep failed (exit ${status}); exclusion unverified" >&2
        exit 1
        ;;
esac
