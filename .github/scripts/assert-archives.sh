#!/usr/bin/env bash
# assert-archives.sh <expected-count> <archive>...
#
# Exits 0 only when: the count matches; every archive's member set is exactly
# `goetia-<target>/{goetia,goetia-shim,README.md,LICENSE.md}`, with `.exe` on
# the two binaries and `/` as the only separator inside a `.zip`; and no two
# binaries anywhere in the call — whether in the same archive or different
# ones — carry byte-identical content.
#
# The member set is checked per archive, derived from that archive's own
# filename — but the binary bytes are tracked across every archive in the
# call. That is what catches a staging step that lost its `--target` and
# copied one host's binary into every row (each archive's filename would
# still look right, and a per-archive check alone would pass every one of
# them), as well as a staging step that copied `goetia` over `goetia-shim`
# (or vice versa) within a single archive.
set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "::error::usage: assert-archives.sh <expected-count> <archive>..." >&2
    exit 1
fi

expected_count="$1"
shift

if [[ ! "$expected_count" =~ ^[1-9][0-9]*$ ]]; then
    echo "::error::expected-count must be a positive integer, got '${expected_count}'" >&2
    exit 1
fi

if [[ "$#" -ne "$expected_count" ]]; then
    echo "::error::expected ${expected_count} archive(s), got $#: ${*:-<none>}" >&2
    exit 1
fi

# list_members <kind> <archive> — one member path per line. Directory
# entries are excluded: both `tar` and a directory-based `zip` emit a
# `goetia-<target>/` entry alongside the four files, and it is not part of
# the member set this script checks.
#
# Listing and filtering are separate steps, and only the listing decides
# failure. Piping `tar`/`unzip` straight into `grep -v` would make an
# archive whose entries are ALL directories (a staging step that created
# `goetia-<target>/` and copied nothing into it) exit 1 from `grep`'s
# no-matches, which under `pipefail` reads as an unreadable archive. That
# archive is perfectly readable and has zero members, which the caller's
# member-set comparison reports far more usefully.
list_members() {
    local kind="$1" archive="$2" listing filtered grep_status=0
    if [[ "$kind" == tar ]]; then
        listing="$(tar -tf "$archive")" || return $?
    else
        listing="$(unzip -Z1 "$archive")" || return $?
    fi

    [[ -n "$listing" ]] || return 0

    filtered="$(grep -v '/$' <<<"$listing")" || grep_status=$?
    # 1 is "no lines left"; anything above it is grep itself failing.
    if [[ "$grep_status" -gt 1 ]]; then
        return "$grep_status"
    fi

    [[ -n "$filtered" ]] || return 0
    printf '%s\n' "$filtered"
}

# extract_member <kind> <archive> <member> — streams the member's bytes to stdout.
extract_member() {
    local kind="$1" archive="$2" member="$3"
    if [[ "$kind" == tar ]]; then
        tar -xOf "$archive" "$member"
    else
        unzip -p "$archive" "$member"
    fi
}

declare -A binary_owner=()  # sha256 -> "archive:member" of the first archive seen with that hash

for archive in "$@"; do
    if [[ ! -f "$archive" ]]; then
        echo "::error::archive not found: ${archive}" >&2
        exit 1
    fi

    base="$(basename -- "$archive")"
    case "$base" in
        *.tar.xz)
            kind=tar
            stem="${base%.tar.xz}"
            ;;
        *.zip)
            kind=zip
            stem="${base%.zip}"
            ;;
        *)
            echo "::error::${archive}: unrecognized extension (expected .tar.xz or .zip)" >&2
            exit 1
            ;;
    esac

    case "$stem" in
        goetia-*) ;;
        *)
            echo "::error::${archive}: filename must start with 'goetia-', got '${base}'" >&2
            exit 1
            ;;
    esac
    target="${stem#goetia-}"
    prefix="goetia-${target}"

    ext=""
    if [[ "$kind" == zip ]]; then
        ext=".exe"
    fi

    # A plain `mapfile -t members < <(list_members ...)` would swallow a
    # failure inside the process substitution: `mapfile`'s own exit status
    # only reflects reading from the pipe, not the command that fed it, so a
    # corrupt archive would silently yield a truncated member list instead
    # of surfacing the real `tar`/`unzip` error. Capture the listing via a
    # command substitution instead, whose exit status *is* the pipeline's
    # (thanks to the script-wide `pipefail`), and fail loudly on it.
    members_listing=""
    if ! members_listing="$(list_members "$kind" "$archive")"; then
        echo "::error::${archive}: failed to list members (corrupt or unreadable archive?)" >&2
        exit 1
    fi
    members=()
    if [[ -n "$members_listing" ]]; then
        mapfile -t members <<< "$members_listing"
    fi

    if [[ "$kind" == zip ]]; then
        for member in "${members[@]}"; do
            case "$member" in
                *\\*)
                    echo "::error::${archive}: entry '${member}' uses a backslash path separator; zip entries must use '/'" >&2
                    exit 1
                    ;;
            esac
        done
    fi

    expected_members=(
        "${prefix}/goetia${ext}"
        "${prefix}/goetia-shim${ext}"
        "${prefix}/README.md"
        "${prefix}/LICENSE.md"
    )

    mapfile -t actual_sorted < <(printf '%s\n' "${members[@]}" | sort)
    mapfile -t expected_sorted < <(printf '%s\n' "${expected_members[@]}" | sort)

    # Compared element by element, not via "${arr[*]}" IFS-joined strings:
    # `${arr[*]}` joins on IFS's first character (a space), so a set of N
    # elements where one member's name happens to contain a space can join
    # to the exact same string as a different-length set — ("a b") and ("a"
    # "b") compare equal as strings even though they are different sets.
    # A 3-member archive whose third member is literally named
    # "<prefix>/LICENSE.md <prefix>/README.md" (one member, embedded space)
    # joins byte-for-byte identical to the real 4-member expected set.
    mismatch=0
    if [[ "${#actual_sorted[@]}" -ne "${#expected_sorted[@]}" ]]; then
        mismatch=1
    else
        for ((i = 0; i < ${#actual_sorted[@]}; i++)); do
            if [[ "${actual_sorted[$i]}" != "${expected_sorted[$i]}" ]]; then
                mismatch=1
                break
            fi
        done
    fi

    if [[ "$mismatch" -eq 1 ]]; then
        echo "::error::${archive}: member set mismatch" >&2
        echo "  expected: ${expected_sorted[*]}" >&2
        echo "  actual:   ${actual_sorted[*]}" >&2
        exit 1
    fi

    for name in goetia goetia-shim; do
        member="${prefix}/${name}${ext}"
        # Guarded the same way the `list_members` call above is: a member
        # can pass listing (which reads only the central directory / tar
        # headers) and still fail to extract (e.g. a corrupted zip entry
        # fails its CRC check on `unzip -p`, but not on `unzip -Z1`).
        # `set -e` + `pipefail` already fails the script closed on that —
        # correct — but with no annotation, so the log shows only the raw
        # `tar`/`unzip` diagnostic and no `::error::` line.
        hash=""
        if ! hash="$(extract_member "$kind" "$archive" "$member" | sha256sum | cut -d' ' -f1)"; then
            echo "::error::${archive}: failed to extract member '${member}' (corrupt or unreadable archive?)" >&2
            exit 1
        fi
        owner="${binary_owner[$hash]-}"
        if [[ -n "$owner" ]]; then
            if [[ "${owner%%:*}" == "$archive" ]]; then
                echo "::error::${archive}:${member} is byte-identical to ${owner#*:} in the same archive — goetia and goetia-shim must be different programs; check the staging step" >&2
            else
                echo "::error::${archive}:${member} is byte-identical to ${owner} — two archives should never ship the same binary; check the staging step's --target" >&2
            fi
            exit 1
        fi
        binary_owner["$hash"]="${archive}:${member}"
    done
done

echo "all ${expected_count} archive(s) verified"
