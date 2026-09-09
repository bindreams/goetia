#!/usr/bin/env bash
# resolve-draft-release.sh <tag>
#
# Prints `sha=<commit>` for the draft release <tag>, and fails if <tag> is
# not a draft this pipeline could have created. Everything downstream — the
# checkout, the manifest check, the attestation's commit pin, the tag — is
# built on that commit, so a wrong or unreadable answer must stop the run
# rather than be guessed at.
#
# `targetCommitish` is whatever the release was created with. `gh release
# create --target "$GITHUB_SHA"` stores a commit; a hand-made draft can
# store a branch name, which resolves differently on every read and would
# make the tag bind to whatever main happened to be at publish time.
set -euo pipefail

# `gh` is resolved through GOETIA_GH so the test suite can point it at a
# stub. See create-release-tag.sh.
gh_bin="${GOETIA_GH:-gh}"

tag="${1:?tag required}"

if ! release_json="$("$gh_bin" release view "$tag" --json isDraft,targetCommitish)"; then
    echo "::error::gh release view failed for ${tag}. Does the draft release exist?" >&2
    exit 1
fi

is_draft="$(jq -r '.isDraft' <<<"$release_json")"
if [[ "$is_draft" != "true" ]]; then
    echo "::error::Release ${tag} is not a draft (isDraft=${is_draft}). This workflow only publishes drafts created by Draft Release." >&2
    exit 1
fi

sha="$(jq -r '.targetCommitish' <<<"$release_json")"
if [[ ! "$sha" =~ ^[0-9a-f]{40}$ ]]; then
    echo "::error::Draft ${tag} targets '${sha}', not a 40-character commit SHA. Recreate it via Draft Release." >&2
    exit 1
fi

echo "sha=${sha}"
