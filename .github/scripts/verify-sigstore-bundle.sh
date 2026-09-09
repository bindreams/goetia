#!/usr/bin/env bash
# verify-sigstore-bundle.sh <bundle> <commit-sha> <archive>...
#
# Verifies the release's Sigstore bundle against every archive it is given.
# Both release stages call this: Draft Release checks what it just attested,
# Publish Release re-checks the bytes it downloaded from the draft
# immediately before the irreversible publish. One implementation, so the
# two can never drift.
#
# The pins are baked in rather than passed by the caller, for the same
# reason: they describe the workflow that MINTS the bundle (Draft Release on
# refs/heads/main), so they are the same at both call sites, and a pin that
# is a parameter is a pin that one caller can quietly weaken.
#
# `--sha` is the only commit-specific pin among them. Without it the other
# five are satisfied by any bundle from any Draft Release dispatch on main —
# so a whole archive set plus its bundle lifted from a previous release
# verifies green, and the release publishes binaries built from a different
# commit than it claims.
#
# A bundle verifies against exactly one input at a time (`--bundle` with two
# file arguments exits non-zero: "can only be used with a single input file
# or digest"), so this is a loop rather than one command, and every archive
# is checked even after one fails — a release operator wants the whole list,
# not the first entry. Success is decided by exit status, never by output:
# `sigstore` writes `OK: <file>` to stderr and the in-toto statement to
# stdout, so grepping stdout for OK would never match and would report
# success on every run, including failed ones.
set -euo pipefail

# The verifier is resolved through `SIGSTORE_BIN` so the test suite can point
# it at a stub. PATH is not a reliable hook for this one: `uv tool install
# sigstore` puts the real binary in `~/.local/bin`, which shell profiles
# prepend, so a stub directory prepended by the caller can silently lose.
sigstore_bin="${SIGSTORE_BIN:-sigstore}"

cert_identity="https://github.com/bindreams/goetia/.github/workflows/draft-release.yaml@refs/heads/main"
repository="bindreams/goetia"
ref="refs/heads/main"
workflow_name="Draft Release"
trigger="workflow_dispatch"

# Three arguments minimum: a bundle, a commit, and at least one archive. An
# empty `nullglob` expansion at the call site lands here as zero archives,
# and a loop over zero archives exits 0 having verified nothing.
if [[ $# -lt 3 ]]; then
    echo "::error::usage: verify-sigstore-bundle.sh <bundle> <commit-sha> <archive>..." >&2
    exit 1
fi

bundle="$1"
sha="$2"
shift 2

if [[ ! -f "$bundle" ]]; then
    echo "::error::bundle not found: ${bundle}" >&2
    exit 1
fi

if [[ ! "$sha" =~ ^[0-9a-f]{40}$ ]]; then
    echo "::error::commit must be a 40-character lowercase SHA, got '${sha}'" >&2
    exit 1
fi

for archive in "$@"; do
    if [[ ! -f "$archive" ]]; then
        echo "::error::archive not found: ${archive}" >&2
        exit 1
    fi
done

status=0
for archive in "$@"; do
    if ! "$sigstore_bin" verify github \
        --bundle "$bundle" \
        --cert-identity "$cert_identity" \
        --repository "$repository" \
        --ref "$ref" \
        --name "$workflow_name" \
        --trigger "$trigger" \
        --sha "$sha" \
        "$archive"; then
        echo "::error::sigstore verification failed for ${archive}" >&2
        status=1
    fi
done

if [[ "$status" -ne 0 ]]; then
    exit "$status"
fi

echo "all $# archive(s) verified against ${bundle} at ${sha}"
