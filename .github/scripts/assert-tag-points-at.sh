#!/usr/bin/env bash
# assert-tag-points-at.sh <tag> <sha>
#
# Resolves <tag> on the repository named by $GH_REPO and fails unless it is
# <sha>. Runs after the release is published, so both failure paths report a
# state that cannot be undone and must not be "cleaned up" on the strength
# of this error alone: deleting a published release permanently reserves its
# tag name, and the crate is already on crates.io.
#
# The release's `targetCommitish` is NOT usable for this: it is the stored
# creation input, which GitHub never recomputes when a draft is published,
# so it would echo back the intended commit and this guard would never fire.
# Resolving the tag is what reads the published reality.
set -euo pipefail

# `gh` is resolved through GOETIA_GH so the test suite can point it at a
# stub. See create-release-tag.sh.
gh_bin="${GOETIA_GH:-gh}"

tag="${1:?tag required}"
sha="${2:?commit sha required}"
repo="${GH_REPO:?GH_REPO must name the repository}"

if ! published="$("$gh_bin" api "repos/${repo}/commits/${tag}" --jq '.sha')"; then
    echo "::error::Could not resolve tag ${tag} after publishing." >&2
    echo "::error::The crate is on crates.io and the GitHub release is published; both are irreversible." >&2
    echo "::error::Verify by hand that ${tag} points at ${sha}. Do NOT delete the release or the tag on the strength of this error alone." >&2
    exit 1
fi

if [[ "$published" != "$sha" ]]; then
    echo "::error::Release ${tag} points at ${published}, not the published commit ${sha}. The release is already published." >&2
    echo "::error::Fix by hand. Do NOT delete the release or the tag on the strength of this error alone." >&2
    exit 1
fi

echo "${tag} points at ${sha}"
