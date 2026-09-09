#!/usr/bin/env bash
# create-release-tag.sh <tag> <sha>
#
# Creates `refs/tags/<tag>` at <sha> on the repository named by $GH_REPO, and
# fails if that ref already exists.
#
# Un-drafting a release is what creates its tag, and GitHub binds a release
# to a tag that already exists rather than to the commit the draft was
# targeted at. Checking the tag's absence and then publishing leaves a window
# in which a tag pushed by anyone else captures the release — the check
# cannot close it, only narrow it, and the read-back afterwards can do
# nothing but report a published, immutable release pointing at a stranger's
# commit.
#
# `POST /repos/{owner}/{repo}/git/refs` names the ref and its commit in one
# request and answers 422 "Reference already exists" rather than moving an
# existing ref, so it is a compare-and-swap: after it succeeds, the tag
# exists at this commit and cannot be moved out from under the publish.
# (Publishing a release onto an already-existing tag is permitted with
# immutable releases enabled — goreleaser/goreleaser publishes exactly that
# way, from `on: push: tags:`, and its releases report isImmutable: true.)
#
# The REST create is used rather than `git push origin <sha>:refs/tags/<tag>`
# because it needs no local objects and no persisted push credentials: it is
# the same atomicity from a shallow checkout, over the token this job
# already holds.
#
# A refusal is unconditional, including when the existing tag happens to
# point at this very commit. Reading it back to decide would be another
# check-then-act, on a ref that can still be force-moved before the publish.
set -euo pipefail

# `gh` is resolved through GOETIA_GH so the test suite can point it at a stub
# with no chance of a real ref being created.
gh_bin="${GOETIA_GH:-gh}"

if [[ $# -ne 2 ]]; then
    echo "::error::usage: create-release-tag.sh <tag> <sha>" >&2
    exit 1
fi

tag="$1"
sha="$2"
repo="${GH_REPO:-}"

if [[ -z "$repo" ]]; then
    echo "::error::GH_REPO must name the repository to create the tag in" >&2
    exit 1
fi

if [[ ! "$sha" =~ ^[0-9a-f]{40}$ ]]; then
    echo "::error::commit must be a 40-character lowercase SHA, got '${sha}'" >&2
    exit 1
fi

output=""
status=0
output="$("$gh_bin" api -X POST "repos/${repo}/git/refs" \
    -f "ref=refs/tags/${tag}" -f "sha=${sha}" 2>&1)" || status=$?

if [[ "$status" -eq 0 ]]; then
    echo "created refs/tags/${tag} at ${sha}"
    exit 0
fi

if grep -qi 'already exists' <<<"$output"; then
    echo "::error::Tag ${tag} already exists on ${repo}; refusing to publish a release onto a tag this run did not create." >&2
    echo "::error::Inspect it with: gh api -X GET repos/${repo}/git/ref/tags/${tag}" >&2
    echo "::error::If it is the leftover of an earlier run of this workflow that failed after creating the tag, no release is published against it yet, so it can be deleted and this workflow re-dispatched." >&2
    exit 1
fi

echo "::error::could not create tag ${tag} at ${sha}: ${output}" >&2
exit 1
