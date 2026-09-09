#!/usr/bin/env bash
# Echo a bare MAJOR.MINOR.PATCH from a dispatch input, stripping one optional
# leading `v`. Exits non-zero on anything else, including a leading zero in any
# component: `v01.2.3` and `v1.2.3` are different git tags.
set -euo pipefail

input="${1-}"
version="${input#v}"

if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    echo "::error::Invalid version: '${input}'" >&2
    exit 1
fi

printf '%s\n' "$version"
