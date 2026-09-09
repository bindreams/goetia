#!/usr/bin/env bash
# check-crates-io-version.sh <crate> <version> <packaged-crate-file>
#
# Decides whether `cargo publish` still needs to run, and prints exactly one
# line to stdout for the workflow to append to $GITHUB_OUTPUT:
#
#     already_published=true    the version on crates.io IS this artifact
#     already_published=false   the version is not on crates.io
#
# Everything else it has to say goes to stderr. Any answer it cannot justify
# is a failure, not a skip.
#
# This is what makes the publish idempotent, and idempotence is what makes
# the whole job re-runnable. A second `cargo publish` of an existing version
# fails, and relying on that failure being benign would mean parsing cargo's
# message to decide whether a red run is really red.
#
# But a 200 does not mean "my own prior run published this". Yanking is
# something only a human does, so a yanked version was never this pipeline;
# and a version can equally have been published by hand, or by an earlier
# run of this workflow from a DIFFERENT commit — run 1 publishes 0.1.0 from
# commit X and then fails before flipping the release, the operator deletes
# the draft and re-drafts from a newer commit Y that still declares 0.1.0,
# and every check upstream of here passes. Skipping the publish on the
# strength of the 200 alone would leave crates.io serving commit X while the
# immutable, attested GitHub release claims commit Y — permanently, since a
# version can be yanked but never unpublished.
#
# So a 200 has to prove identity: crates.io's `.version.checksum` is the
# sha256 of the uploaded `.crate`, and `cargo package --locked` rebuilds
# that same artifact from the commit being released. Equal digests mean the
# published version is byte-for-byte what this run would publish.
set -euo pipefail

# `curl` is resolved through GOETIA_CURL so the test suite can point it at a
# stub with no chance of a real request escaping to crates.io.
curl_bin="${GOETIA_CURL:-curl}"

# crates.io answers 403 to a request with no User-Agent.
user_agent="goetia-release-pipeline (https://github.com/bindreams/goetia)"

if [[ $# -ne 3 ]]; then
    echo "::error::usage: check-crates-io-version.sh <crate> <version> <packaged-crate-file>" >&2
    exit 1
fi

crate="$1"
version="$2"
crate_file="$3"

if [[ ! -f "$crate_file" ]]; then
    echo "::error::packaged crate not found: ${crate_file}. Run \`cargo package --locked\` first." >&2
    exit 1
fi

response=""
if ! response="$("$curl_bin" -sS -X GET -L -A "$user_agent" -w '\n%{http_code}' \
    "https://crates.io/api/v1/crates/${crate}/${version}")"; then
    echo "::error::crates.io API request failed; cannot confirm whether ${crate} ${version} is already published." >&2
    exit 1
fi

code="${response##*$'\n'}"
body="${response%$'\n'*}"

case "$code" in
    404)
        echo "${crate} ${version} is not on crates.io" >&2
        echo "already_published=false"
        ;;
    200)
        yanked="$(jq -r '.version.yanked' <<<"$body")"
        case "$yanked" in
            true)
                echo "::error::Version ${version} exists on crates.io but is YANKED. This pipeline never yanks, so this was not its own prior run — investigate by hand before re-dispatching." >&2
                exit 1
                ;;
            false) ;;
            *)
                echo "::error::crates.io answered 200 for ${version} but its \`.version.yanked\` is '${yanked}'; cannot tell whether the version is live." >&2
                exit 1
                ;;
        esac

        checksum="$(jq -r '.version.checksum' <<<"$body")"
        if [[ ! "$checksum" =~ ^[0-9a-f]{64}$ ]]; then
            echo "::error::crates.io answered 200 for ${version} but its \`.version.checksum\` is '${checksum}', not a sha256; cannot prove what is published." >&2
            exit 1
        fi

        local_checksum="$(sha256sum "$crate_file" | cut -d' ' -f1)"
        if [[ "$local_checksum" != "$checksum" ]]; then
            echo "::error::Version ${version} is on crates.io, but its checksum ${checksum} does not match the crate this commit packages (${local_checksum})." >&2
            echo "::error::Skipping the publish would leave crates.io serving different code than this release claims, and neither can be undone." >&2
            echo "::error::Investigate by hand: download https://crates.io/api/v1/crates/${crate}/${version}/download and compare its \`.cargo_vcs_info.json\` against this release's commit. A published version from a different commit and a published version packaged by a different cargo release both land here." >&2
            exit 1
        fi

        echo "${crate} ${version} is already on crates.io and byte-identical to this commit's package; skipping publish" >&2
        echo "already_published=true"
        ;;
    *)
        echo "::error::Unexpected HTTP ${code} from the crates.io API; cannot confirm whether ${version} is already published." >&2
        exit 1
        ;;
esac
