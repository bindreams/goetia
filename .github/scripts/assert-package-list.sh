#!/usr/bin/env bash
# assert-package-list.sh <listing-file>
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
set -euo pipefail

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

# goetia's exclude list (Cargo.toml, Task 2 step 3): directory prefixes
# match any path under them; the four single files are anchored at both
# ends.
excluded_pattern='^(\.github/|\.claude/|scripts/|prek\.toml$|\.editorconfig$|\.gitattributes$|\.rustfmt\.toml$)'

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
