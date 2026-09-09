#!/usr/bin/env bash
# Succeed only when `origin` provably has no tag `$1`.
#
# The three-way branch is the point. `git ls-remote --exit-code` returns 0 when
# the ref exists and 2 when it does not; anything else is a transport, auth or
# protocol failure. Collapsing those into "absent" would let an outage green-light
# a release, and an existing tag silently captures `gh release create --target`.
set -euo pipefail

tag="${1:?tag required}"

status=0
git ls-remote --exit-code --tags origin "refs/tags/${tag}" >/dev/null 2>&1 || status=$?

case "$status" in
    0)
        echo "::error::Tag ${tag} already exists on origin. GitHub would bind the release to that tag's existing commit instead of the intended one. Delete the tag first."
        exit 1
        ;;
    2)
        echo "no existing tag ${tag}"
        ;;
    *)
        echo "::error::git ls-remote failed (exit ${status}); cannot confirm ${tag} is absent"
        exit 1
        ;;
esac
