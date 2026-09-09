#!/usr/bin/env bash
# assert-commit-on-main.sh <sha>
#
# Succeed only when `origin/main` provably contains <sha>. workflow_dispatch
# accepts any `ref`, so without this a release can be cut from unmerged code
# while every later check still passes.
#
# The three-way branch is the point, the same one assert-tag-absent.sh
# makes: `git merge-base --is-ancestor` exits 0 when the commit is an
# ancestor and 1 when it is not; anything else is git itself failing, and
# collapsing that into "not an ancestor" (abort, harmless) or "ancestor"
# (release unmerged code) are both answers to a question that was never
# answered. The fetch is checked for the same reason — a stale or missing
# origin/main would otherwise be compared against.
set -euo pipefail

sha="${1:?commit sha required}"

if ! git fetch --quiet origin main; then
    echo "::error::git fetch origin main failed; cannot confirm ${sha} is on main"
    exit 1
fi

status=0
git merge-base --is-ancestor "$sha" origin/main || status=$?

case "$status" in
    0)
        echo "${sha} is an ancestor of origin/main"
        ;;
    1)
        echo "::error::${sha} is not an ancestor of origin/main. Release only from merged commits."
        exit 1
        ;;
    *)
        echo "::error::git merge-base failed (exit ${status}); cannot confirm ${sha} is on main"
        exit 1
        ;;
esac
