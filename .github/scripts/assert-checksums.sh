#!/usr/bin/env bash
# assert-checksums.sh <sums-file> <file>...
#
# Exits 0 only when <sums-file> records a matching hash for every one of
# <file>..., and names nothing else.
#
# `sha256sum -c --strict` alone is not that check. `--strict` fails on a
# malformed line, not on a missing one, so it verifies exactly the lines the
# file happens to contain: a SHA256SUMS truncated to one of five archives
# passes cleanly, reporting `goetia-...: OK` while four archives go
# unchecked. SHA256SUMS is unattested by design and mutable between the
# release stages, and it ships as a release asset README.md tells consumers
# to compare their downloads against, so the coverage assertion below is
# what makes verifying it mean anything.
set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "::error::usage: assert-checksums.sh <sums-file> <file>..." >&2
    exit 1
fi

sums="$1"
shift

if [[ ! -f "$sums" ]]; then
    echo "::error::checksum file not found: ${sums}" >&2
    exit 1
fi

# Names are read at a fixed offset — 64 hex digits, a space, and a mode
# character — rather than by splitting on whitespace, so a filename
# containing spaces is read whole instead of being truncated to its first
# word. A leading backslash is sha256sum's escaping marker for a name
# containing a backslash or a newline; nothing in a release is named that,
# and guessing at the unescaping would be worse than refusing.
#
# `|| [[ -n "$line" ]]` keeps the last line when the file does not end in a
# newline: `read` returns non-zero there, and dropping that line would hide
# whatever it names from the coverage check below.
listed=()
while IFS= read -r line || [[ -n "$line" ]]; do
    if [[ ! "$line" =~ ^[0-9a-f]{64}\ [\ *] ]]; then
        echo "::error::${sums}: unparseable line: ${line}" >&2
        exit 1
    fi
    listed+=("${line:66}")
done < "$sums"

mapfile -t listed_sorted < <(printf '%s\n' "${listed[@]}" | sort)
mapfile -t expected_sorted < <(printf '%s\n' "$@" | sort)

mismatch=0
if [[ "${#listed_sorted[@]}" -ne "${#expected_sorted[@]}" ]]; then
    mismatch=1
else
    for ((i = 0; i < ${#listed_sorted[@]}; i++)); do
        if [[ "${listed_sorted[$i]}" != "${expected_sorted[$i]}" ]]; then
            mismatch=1
            break
        fi
    done
fi

if [[ "$mismatch" -eq 1 ]]; then
    echo "::error::${sums} does not name exactly the files it was asked to cover" >&2
    echo "  expected: ${expected_sorted[*]}" >&2
    echo "  listed:   ${listed_sorted[*]}" >&2
    exit 1
fi

# Hashing last: a coverage failure above is a more precise diagnosis than
# whatever `sha256sum -c` would say about the same file, and there is no
# point hashing a set that was never the right set.
if ! sha256sum -c --strict -- "$sums"; then
    echo "::error::${sums}: recorded hash does not match the file on disk" >&2
    exit 1
fi

echo "${sums} covers all $# file(s), and every hash matches"
