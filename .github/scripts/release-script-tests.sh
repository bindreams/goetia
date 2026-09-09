#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
normalize="${script_dir}/normalize-version.sh"
assert_tag="${script_dir}/assert-tag-absent.sh"
assert_archives="${script_dir}/assert-archives.sh"
assert_package_list="${script_dir}/assert-package-list.sh"
assert_asset_count="${script_dir}/assert-asset-count.sh"
assert_test_binaries="${script_dir}/assert-test-binaries.sh"
failures=0

# Asserts BOTH stdout and exit status. Checking stdout alone would pass a
# script that printed the right version and then exited non-zero — which the
# workflows consume under `set -euo pipefail`, so it would halt a release.
assert_accepts() {
    local description="$1" expected="$2" input="$3"
    local actual status=0
    actual="$(bash "$normalize" "$input" 2>/dev/null)" || status=$?
    if [[ "$status" -eq 0 && "$actual" == "$expected" ]]; then
        echo "ok   - ${description}"
    else
        echo "FAIL - ${description} (expected '${expected}' status 0, got '${actual}' status ${status})"
        failures=$((failures + 1))
    fi
}

assert_rejects() {
    local description="$1" input="$2"
    if bash "$normalize" "$input" >/dev/null 2>&1; then
        echo "FAIL - ${description} (expected rejection, got success)"
        failures=$((failures + 1))
    else
        echo "ok   - ${description}"
    fi
}

# normalize-version.sh -----------------------------------------------------

assert_accepts "passes a bare semver core"    "0.1.0"    "0.1.0"
assert_accepts "strips a leading v"           "0.1.0"    "v0.1.0"
assert_accepts "accepts multi-digit parts"    "10.20.30" "10.20.30"
assert_rejects "rejects a prerelease suffix"  "1.2.3-rc1"
assert_rejects "rejects build metadata"       "1.2.3+build"
assert_rejects "rejects two components"       "0.1"
assert_rejects "rejects four components"      "0.1.0.0"
assert_rejects "rejects an empty string"      ""
assert_rejects "rejects trailing whitespace"  "0.1.0 "
assert_rejects "rejects a command injection"  '0.1.0; rm -rf /'
assert_rejects "rejects a doubled v"          "vv0.1.0"
# v01.2.3 and v1.2.3 are different git tags, so a non-canonical component
# would silently produce a tag nobody expects.
assert_rejects "rejects a leading zero"       "01.2.3"
assert_rejects "rejects an inner leading zero" "1.02.3"

# assert-tag-absent.sh -----------------------------------------------------
# Drives the script against a stub `git` that exits with a chosen status, so
# all three branches are exercised without touching a real remote. The stub
# also records its own argv and only honors the configured exit code when
# invoked with the exact `ls-remote --exit-code --tags origin refs/tags/<ref>`
# shape assert-tag-absent.sh is supposed to use — any other invocation is
# treated as a stub-usage bug (exit 97, a code the script's own case
# statement can't confuse with a real git outcome). Without this, a stub
# that answers regardless of its arguments only pins exit-code mapping, and
# would keep passing even if the script queried the wrong ref or none at all.

stub_dir="$(mktemp -d)"
archive_root="$(mktemp -d)"
pkg_list_root="$(mktemp -d)"
asset_count_root="$(mktemp -d)"
test_binaries_root="$(mktemp -d)"
trap 'rm -rf "$stub_dir" "$archive_root" "$pkg_list_root" "$asset_count_root" "$test_binaries_root"' EXIT

make_git_stub() {
    local exit_code="$1" expected_ref="$2"
    cat > "${stub_dir}/git" <<EOF
#!/usr/bin/env bash
echo "\$@" > "${stub_dir}/git.invocation"
if [[ "\$*" == "ls-remote --exit-code --tags origin ${expected_ref}" ]]; then
    exit ${exit_code}
else
    exit 97
fi
EOF
    chmod +x "${stub_dir}/git"
}

# assert_git_invocation <expected-argv> — fails the current test unless the
# stub was actually invoked with exactly <expected-argv>. Returns 0 (matched)
# or 1 (mismatch, and already recorded the failure).
assert_git_invocation() {
    local description="$1" expected="$2"
    local actual
    actual="$(cat "${stub_dir}/git.invocation" 2>/dev/null || true)"
    if [[ -z "$actual" ]]; then
        actual="<git never invoked>"
    fi
    if [[ "$actual" != "$expected" ]]; then
        echo "FAIL - ${description} (expected git invoked as '${expected}', got '${actual}')"
        failures=$((failures + 1))
        return 1
    fi
    return 0
}

assert_tag_result() {
    local description="$1" git_exit="$2" want_success="$3"
    make_git_stub "$git_exit" "refs/tags/v0.1.0"
    local succeeded=no
    if PATH="${stub_dir}:${PATH}" bash "$assert_tag" v0.1.0 >/dev/null 2>&1; then
        succeeded=yes
    fi
    assert_git_invocation "$description" "ls-remote --exit-code --tags origin refs/tags/v0.1.0" || return
    if [[ "$succeeded" == "$want_success" ]]; then
        echo "ok   - ${description}"
    else
        echo "FAIL - ${description} (git exit ${git_exit}, wanted success=${want_success})"
        failures=$((failures + 1))
    fi
}

assert_tag_result "tag present (git exit 0) is refused"        0   no
assert_tag_result "tag absent (git exit 2) is accepted"        2   yes
assert_tag_result "transient failure (git exit 1) is refused"  1   no
assert_tag_result "auth failure (git exit 128) is refused"     128 no

# The stub only recognizes refs/tags/v0.1.0; asking assert-tag-absent.sh
# about a different tag must make it query that tag's own ref, not a
# hardcoded one — a script that ignored $1 and always asked about v0.1.0
# would get treated as "absent" here and wrongly succeed.
make_git_stub 2 "refs/tags/v0.1.0"
if PATH="${stub_dir}:${PATH}" bash "$assert_tag" v9.9.9 >/dev/null 2>&1; then
    echo "FAIL - queries the tag it was given, not a hardcoded one (stub only recognizes refs/tags/v0.1.0; script asked about v9.9.9 and still succeeded)"
    failures=$((failures + 1))
else
    assert_git_invocation "queries the tag it was given, not a hardcoded one" "ls-remote --exit-code --tags origin refs/tags/v9.9.9" \
        && echo "ok   - queries the tag it was given, not a hardcoded one"
fi

# assert-archives.sh ---------------------------------------------------------
#
# Fixtures live under $archive_root, cleaned up by the trap above. Every
# fixture-building helper below takes a fresh subdirectory from `mktemp -d -p
# "$archive_root"` so fixtures from different test cases never collide.

# should_omit <name> <omitted...> — true if <name> appears in the rest of the
# args. Iterating "$@" (rather than a named array) keeps this safe under
# `set -u` even when the omit list is empty.
should_omit() {
    local target="$1"
    shift
    local name
    for name in "$@"; do
        if [[ "$name" == "$target" ]]; then
            return 0
        fi
    done
    return 1
}

# write_members <dir> <goetia-content> <shim-content> [omit...]
# Populates <dir> with goetia, goetia-shim, README.md and LICENSE.md,
# skipping any name passed in the trailing omit list.
write_members() {
    local dir="$1" goetia_content="$2" shim_content="$3"
    shift 3
    mkdir -p "$dir"

    local name
    for name in goetia goetia-shim README.md LICENSE.md; do
        if should_omit "$name" "$@"; then
            continue
        fi
        case "$name" in
            goetia) printf '%s' "$goetia_content" > "${dir}/${name}" ;;
            goetia-shim) printf '%s' "$shim_content" > "${dir}/${name}" ;;
            *) printf '%s content\n' "$name" > "${dir}/${name}" ;;
        esac
    done
}

# make_tar_archive <output> <target> <goetia-content> <shim-content> [omit...]
# Builds a `.tar.xz` the same way the workflow does: a single top-level
# `goetia-<target>/` directory, tarred from its parent.
make_tar_archive() {
    local out="$1" target="$2" goetia_content="$3" shim_content="$4"
    shift 4
    local root; root="$(mktemp -d -p "$archive_root")"
    write_members "${root}/goetia-${target}" "$goetia_content" "$shim_content" "$@"
    tar -cJf "$out" -C "$root" "goetia-${target}"
}

# make_tar_with_raw_members <output> <name>=<content> ...
# Writes a `.tar.xz` with each entry's name exactly as given, including a
# literal space or slash inside a single entry — no filesystem staging, so
# the name never has to survive being a real path on disk. Python's
# tarfile takes the member name verbatim, the same way make_zip_entries
# uses zipfile.
make_tar_with_raw_members() {
    local out="$1"
    shift
    python3 - "$out" "$@" <<'PY'
import io
import sys
import tarfile

out = sys.argv[1]
with tarfile.open(out, "w:xz") as archive:
    for pair in sys.argv[2:]:
        name, content = pair.split("=", 1)
        data = content.encode()
        info = tarfile.TarInfo(name=name)
        info.size = len(data)
        archive.addfile(info, io.BytesIO(data))
PY
}

# make_zip_entries <output> <arcname>=<source-file> ...
# Writes a zip with each entry's name exactly as given — including a
# backslash, which no Unix `zip` would ever produce, so `Compress-Archive`'s
# reported behavior has to be hand-crafted instead of built with `zip` and
# assumed. Python's zipfile takes the arcname verbatim.
make_zip_entries() {
    local out="$1"
    shift
    python3 - "$out" "$@" <<'PY'
import sys
import zipfile

out = sys.argv[1]
with zipfile.ZipFile(out, "w") as archive:
    for pair in sys.argv[2:]:
        arcname, path = pair.split("=", 1)
        archive.write(path, arcname)
PY
}

# make_zip_dir_only <output> <dirname>
# Writes a zip whose only entry is the directory `<dirname>/` — a readable,
# well-formed archive with zero file members, which is what a staging step
# that created the target directory and then copied nothing into it
# produces.
make_zip_dir_only() {
    python3 - "$1" "$2" <<'PY'
import sys
import zipfile

out, dirname = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(out, "w") as archive:
    archive.writestr(zipfile.ZipInfo(dirname + "/"), b"")
PY
}

# make_zip_archive <output> <target> <goetia-content> <shim-content> [omit...]
# A well-formed Windows archive: forward-slash separators, `.exe` on both
# binaries.
make_zip_archive() {
    local out="$1" target="$2" goetia_content="$3" shim_content="$4"
    shift 4
    local root; root="$(mktemp -d -p "$archive_root")"
    local staged="${root}/goetia-${target}"
    write_members "$staged" "$goetia_content" "$shim_content" "$@"

    local prefix="goetia-${target}"
    local entries=()
    if [[ -f "${staged}/goetia" ]]; then
        entries+=("${prefix}/goetia.exe=${staged}/goetia")
    fi
    if [[ -f "${staged}/goetia-shim" ]]; then
        entries+=("${prefix}/goetia-shim.exe=${staged}/goetia-shim")
    fi
    if [[ -f "${staged}/README.md" ]]; then
        entries+=("${prefix}/README.md=${staged}/README.md")
    fi
    if [[ -f "${staged}/LICENSE.md" ]]; then
        entries+=("${prefix}/LICENSE.md=${staged}/LICENSE.md")
    fi

    make_zip_entries "$out" "${entries[@]}"
}

assert_archives_ok() {
    local description="$1" expected_count="$2"
    shift 2
    if bash "$assert_archives" "$expected_count" "$@" >/dev/null 2>&1; then
        echo "ok   - ${description}"
    else
        echo "FAIL - ${description} (expected success, got failure)"
        failures=$((failures + 1))
    fi
}

assert_archives_rejects() {
    local description="$1" expected_count="$2"
    shift 2
    if bash "$assert_archives" "$expected_count" "$@" >/dev/null 2>&1; then
        echo "FAIL - ${description} (expected rejection, got success)"
        failures=$((failures + 1))
    else
        echo "ok   - ${description}"
    fi
}

# assert_archives_rejects_matching <description> <expected-count> <message-substring> <archive>...
# Like assert_archives_rejects, but also requires the script's stderr to
# contain <message-substring> — so a rejection for the wrong reason (e.g. a
# misleading "member set mismatch" masking a real listing failure) still
# fails the test.
assert_archives_rejects_matching() {
    local description="$1" expected_count="$2" message="$3"
    shift 3
    local output
    if output="$(bash "$assert_archives" "$expected_count" "$@" 2>&1)"; then
        echo "FAIL - ${description} (expected rejection, got success)"
        failures=$((failures + 1))
    elif [[ "$output" == *"$message"* ]]; then
        echo "ok   - ${description}"
    else
        echo "FAIL - ${description} (expected message containing '${message}', got: ${output})"
        failures=$((failures + 1))
    fi
}

unix_targets=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-apple-darwin aarch64-apple-darwin)
windows_target=x86_64-pc-windows-msvc

# build_complete_set — prints five well-formed archive paths, one per line,
# one per target, each binary's content unique to its (target, role) pair so
# the cross-archive distinctness check never fires by accident.
build_complete_set() {
    local root; root="$(mktemp -d -p "$archive_root")"
    local target archive

    for target in "${unix_targets[@]}"; do
        archive="${root}/goetia-${target}.tar.xz"
        make_tar_archive "$archive" "$target" "goetia bytes for ${target}" "shim bytes for ${target}"
        printf '%s\n' "$archive"
    done

    archive="${root}/goetia-${windows_target}.zip"
    make_zip_archive "$archive" "$windows_target" "goetia bytes for ${windows_target}" "shim bytes for ${windows_target}"
    printf '%s\n' "$archive"
}

mapfile -t complete_set < <(build_complete_set)
assert_archives_ok "accepts a complete set" 5 "${complete_set[@]}"

assert_archives_rejects "rejects a wrong count" 3 "${complete_set[@]}"

# A zero (or otherwise non-positive) expected-count must never be treated as
# "zero archives to check, trivially satisfied" — that turns a workflow's
# empty/miscomputed count variable into a guard that always reports success.
assert_archives_rejects "rejects a zero count" 0
assert_archives_rejects "rejects a non-numeric count" abc
assert_archives_rejects "rejects an empty count" ""

if bash "$assert_archives" >/dev/null 2>&1; then
    echo "FAIL - rejects being called with no arguments at all (expected rejection, got success)"
    failures=$((failures + 1))
else
    echo "ok   - rejects being called with no arguments at all"
fi

root="$(mktemp -d -p "$archive_root")"
archive="${root}/goetia-x86_64-unknown-linux-musl.tar.xz"
make_tar_archive "$archive" x86_64-unknown-linux-musl "goetia bytes" "shim bytes" goetia-shim
assert_archives_rejects "rejects an archive missing goetia-shim" 1 "$archive"

root="$(mktemp -d -p "$archive_root")"
archive="${root}/goetia-x86_64-unknown-linux-musl.tar.xz"
make_tar_archive "$archive" x86_64-unknown-linux-musl "goetia bytes" "shim bytes" goetia
assert_archives_rejects "rejects an archive missing goetia" 1 "$archive"

root="$(mktemp -d -p "$archive_root")"
archive="${root}/goetia-x86_64-unknown-linux-musl.tar.xz"
make_tar_archive "$archive" x86_64-unknown-linux-musl "goetia bytes" "shim bytes" README.md
assert_archives_rejects "rejects an archive missing README.md" 1 "$archive"

root="$(mktemp -d -p "$archive_root")"
archive="${root}/goetia-x86_64-unknown-linux-musl.tar.xz"
make_tar_archive "$archive" x86_64-unknown-linux-musl "goetia bytes" "shim bytes" LICENSE.md
assert_archives_rejects "rejects an archive missing LICENSE.md" 1 "$archive"

root="$(mktemp -d -p "$archive_root")"
flat_staged="${root}/flat"
write_members "$flat_staged" "goetia bytes" "shim bytes"
archive="${root}/goetia-x86_64-unknown-linux-musl.tar.xz"
tar -cJf "$archive" -C "$flat_staged" goetia goetia-shim README.md LICENSE.md
assert_archives_rejects "rejects a flat archive with no goetia-<target>/ prefix" 1 "$archive"

# The same five member-set rejections against the `.zip` branch. It runs
# different listing and extraction commands (`unzip -Z1`/`unzip -p`) and
# interpolates `.exe` into the expected set, so nothing above covers it —
# and Windows is the row most likely to diverge.
for omitted in goetia-shim goetia README.md LICENSE.md; do
    root="$(mktemp -d -p "$archive_root")"
    archive="${root}/goetia-${windows_target}.zip"
    make_zip_archive "$archive" "$windows_target" "goetia bytes" "shim bytes" "$omitted"
    assert_archives_rejects_matching "rejects a zip missing ${omitted}" 1 "member set mismatch" "$archive"
done

root="$(mktemp -d -p "$archive_root")"
staged="${root}/flat"
write_members "$staged" "goetia bytes" "shim bytes"
archive="${root}/goetia-${windows_target}.zip"
make_zip_entries "$archive" \
    "goetia.exe=${staged}/goetia" \
    "goetia-shim.exe=${staged}/goetia-shim" \
    "README.md=${staged}/README.md" \
    "LICENSE.md=${staged}/LICENSE.md"
assert_archives_rejects_matching "rejects a flat zip with no goetia-<target>/ prefix" 1 \
    "member set mismatch" "$archive"

# An archive holding nothing but its own `goetia-<target>/` directory entry
# is valid and readable — it is what a staging step that created the
# directory and copied nothing into it produces. That must be reported as
# the member-set mismatch it is, naming what is missing, not as a corrupt
# or unreadable archive, which would send an operator hunting for the wrong
# bug.
root="$(mktemp -d -p "$archive_root")"
mkdir -p "${root}/stage/goetia-x86_64-unknown-linux-musl"
archive="${root}/goetia-x86_64-unknown-linux-musl.tar.xz"
tar -cJf "$archive" -C "${root}/stage" goetia-x86_64-unknown-linux-musl
assert_archives_rejects_matching "diagnoses a directory-only tar as a member set mismatch" 1 \
    "member set mismatch" "$archive"

root="$(mktemp -d -p "$archive_root")"
archive="${root}/goetia-${windows_target}.zip"
make_zip_dir_only "$archive" "goetia-${windows_target}"
assert_archives_rejects_matching "diagnoses a directory-only zip as a member set mismatch" 1 \
    "member set mismatch" "$archive"

# A tar with no entries at all: same requirement, and the path where the
# member listing is empty rather than filtered down to empty.
root="$(mktemp -d -p "$archive_root")"
archive="${root}/goetia-x86_64-unknown-linux-musl.tar.xz"
tar -cJf "$archive" --files-from /dev/null
assert_archives_rejects_matching "diagnoses an entirely empty tar as a member set mismatch" 1 \
    "member set mismatch" "$archive"

assert_archives_rejects "rejects a named archive that does not exist" 1 \
    "${archive_root}/goetia-x86_64-unknown-linux-musl.tar.xz"

root="$(mktemp -d -p "$archive_root")"
archive="${root}/goetia-x86_64-unknown-linux-musl.tar.gz"
printf 'irrelevant content' > "$archive"
assert_archives_rejects "rejects an archive with an unrecognized extension" 1 "$archive"

root="$(mktemp -d -p "$archive_root")"
archive="${root}/not-goetia-x86_64-unknown-linux-musl.tar.xz"
make_tar_archive "$archive" x86_64-unknown-linux-musl "goetia bytes" "shim bytes"
assert_archives_rejects "rejects a filename that doesn't start with goetia-" 1 "$archive"

# A corrupt archive must fail with the real listing error, not a misleading
# "member set mismatch" — `list_members` runs under `mapfile < <(...)`, and a
# failure inside that process substitution is otherwise swallowed.
root="$(mktemp -d -p "$archive_root")"
archive="${root}/goetia-x86_64-unknown-linux-musl.tar.xz"
printf 'not a real tar.xz stream' > "$archive"
assert_archives_rejects_matching "surfaces the real listing failure for a corrupt archive" 1 "failed to list members" "$archive"

root="$(mktemp -d -p "$archive_root")"
archive="${root}/goetia-${windows_target}.zip"
printf 'not a real zip stream' > "$archive"
assert_archives_rejects_matching "surfaces the real listing failure for a corrupt zip" 1 "failed to list members" "$archive"

# A member can pass LISTING (which only reads zip central-directory headers)
# and still fail to EXTRACT (a corrupted entry fails its CRC check on
# `unzip -p`, not on `unzip -Z1`) — a different failure mode than the
# corrupt-archive cases above, and one `extract_member`'s call site did not
# annotate before this fix (measured against the unpatched script: bare
# `unzip` CRC diagnostic, exit 2, no `::error::` line).
root="$(mktemp -d -p "$archive_root")"
archive="${root}/goetia-${windows_target}.zip"
make_zip_archive "$archive" "$windows_target" "goetia bytes" "shim bytes"
python3 - "$archive" <<'PY'
import sys

path = sys.argv[1]
with open(path, "rb") as f:
    data = bytearray(f.read())
idx = data.find(b"PK\x03\x04")
data[idx + 80] ^= 0xFF
with open(path, "wb") as f:
    f.write(data)
PY
assert_archives_rejects_matching "surfaces the real extraction failure for a CRC-corrupted zip entry" 1 "failed to extract member" "$archive"

root="$(mktemp -d -p "$archive_root")"
staged="${root}/goetia-${windows_target}"
write_members "$staged" "goetia bytes" "shim bytes"
archive="${root}/goetia-${windows_target}.zip"
make_zip_entries "$archive" \
    "goetia-${windows_target}/goetia=${staged}/goetia" \
    "goetia-${windows_target}/goetia-shim=${staged}/goetia-shim" \
    "goetia-${windows_target}/README.md=${staged}/README.md" \
    "goetia-${windows_target}/LICENSE.md=${staged}/LICENSE.md"
assert_archives_rejects "rejects a Windows archive whose members lack .exe" 1 "$archive"

root="$(mktemp -d -p "$archive_root")"
staged="${root}/flat"
write_members "$staged" "goetia bytes" "shim bytes"
archive="${root}/goetia-${windows_target}.zip"
win_prefix="goetia-${windows_target}"
make_zip_entries "$archive" \
    "${win_prefix}\\goetia.exe=${staged}/goetia" \
    "${win_prefix}\\goetia-shim.exe=${staged}/goetia-shim" \
    "${win_prefix}\\README.md=${staged}/README.md" \
    "${win_prefix}\\LICENSE.md=${staged}/LICENSE.md"
assert_archives_rejects "rejects backslash separators in the zip" 1 "$archive"

# Distinctness must also hold *within* one archive: if goetia and
# goetia-shim are byte-identical, the staging step copied one program over
# the other, even though only a single archive is involved.
root="$(mktemp -d -p "$archive_root")"
archive="${root}/goetia-x86_64-unknown-linux-musl.tar.xz"
make_tar_archive "$archive" x86_64-unknown-linux-musl "same bytes" "same bytes"
assert_archives_rejects "rejects an archive whose own goetia and goetia-shim are byte-identical" 1 "$archive"

root="$(mktemp -d -p "$archive_root")"
shared_content="identical goetia bytes"
archive1="${root}/goetia-x86_64-unknown-linux-musl.tar.xz"
make_tar_archive "$archive1" x86_64-unknown-linux-musl "$shared_content" "shim bytes for a"
archive2="${root}/goetia-aarch64-unknown-linux-musl.tar.xz"
make_tar_archive "$archive2" aarch64-unknown-linux-musl "$shared_content" "shim bytes for b"
archive3="${root}/goetia-x86_64-apple-darwin.tar.xz"
make_tar_archive "$archive3" x86_64-apple-darwin "goetia bytes for c" "shim bytes for c"
archive4="${root}/goetia-aarch64-apple-darwin.tar.xz"
make_tar_archive "$archive4" aarch64-apple-darwin "goetia bytes for d" "shim bytes for d"
archive5="${root}/goetia-${windows_target}.zip"
make_zip_archive "$archive5" "$windows_target" "goetia bytes for e" "shim bytes for e"
assert_archives_rejects "rejects two archives holding identical binaries" 5 \
    "$archive1" "$archive2" "$archive3" "$archive4" "$archive5"

# Reproduces the IFS-joined-string collision directly: a 3-member archive
# (goetia, goetia-shim, and ONE member literally named
# "<prefix>/LICENSE.md <prefix>/README.md" with an embedded space) is
# missing LICENSE.md and README.md as their own members, but its sorted
# member list joins via "${arr[*]}" to the exact same string as the real
# 4-member expected set. Before the element-by-element comparison, this
# archive verified successfully — measured directly against the unpatched
# script: `all 1 archive(s) verified`, exit 0.
root="$(mktemp -d -p "$archive_root")"
archive="${root}/goetia-x86_64-unknown-linux-musl.tar.xz"
prefix="goetia-x86_64-unknown-linux-musl"
make_tar_with_raw_members "$archive" \
    "${prefix}/goetia=goetia bytes" \
    "${prefix}/goetia-shim=shim bytes" \
    "${prefix}/LICENSE.md ${prefix}/README.md=combined bytes"
assert_archives_rejects_matching "rejects a member set whose IFS-joined string collides with the expected set" 1 \
    "member set mismatch" "$archive"

# assert-package-list.sh -----------------------------------------------------
#
# Fixture listings live under $pkg_list_root, cleaned up by the trap above.

# write_package_listing <name> <line>... — writes one `cargo package --list`
# fixture, one path per line, and prints its path.
write_package_listing() {
    local name="$1"
    shift
    local file="${pkg_list_root}/${name}"
    printf '%s\n' "$@" > "$file"
    printf '%s\n' "$file"
}

# A well-formed listing: both required-present paths, both [[bin]] sources,
# a few ordinary source files, and nothing from an excluded prefix.
complete_listing_lines=(
    "Cargo.toml"
    "Cargo.lock"
    "LICENSE.md"
    "README.md"
    "src/lib.rs"
    "src/main.rs"
    "src/bin/shim/main.rs"
    "src/backend/launchd.rs"
)

assert_package_list_ok() {
    local description="$1" listing="$2"
    if bash "$assert_package_list" "$listing" >/dev/null 2>&1; then
        echo "ok   - ${description}"
    else
        echo "FAIL - ${description} (expected success, got failure)"
        failures=$((failures + 1))
    fi
}

# assert_package_list_rejects_matching <description> <listing> <message-substring>
# Requires both rejection and that stderr names the real reason — a
# rejection for the wrong reason (e.g. reporting a leaked path when a
# required path was actually the one missing) still fails the test.
assert_package_list_rejects_matching() {
    local description="$1" listing="$2" message="$3"
    local output
    if output="$(bash "$assert_package_list" "$listing" 2>&1)"; then
        echo "FAIL - ${description} (expected rejection, got success)"
        failures=$((failures + 1))
    elif [[ "$output" == *"$message"* ]]; then
        echo "ok   - ${description}"
    else
        echo "FAIL - ${description} (expected message containing '${message}', got: ${output})"
        failures=$((failures + 1))
    fi
}

listing="$(write_package_listing complete "${complete_listing_lines[@]}")"
assert_package_list_ok "accepts a well-formed listing" "$listing"

# Positive control: each required-present path missing must be caught on its
# own terms, not misreported as a leaked exclusion.
listing="$(write_package_listing missing-cargo-toml Cargo.lock LICENSE.md README.md src/lib.rs src/main.rs src/bin/shim/main.rs)"
assert_package_list_rejects_matching "rejects a listing missing Cargo.toml" "$listing" "Cargo.toml"

listing="$(write_package_listing missing-lib-rs Cargo.toml Cargo.lock LICENSE.md README.md src/main.rs src/bin/shim/main.rs)"
assert_package_list_rejects_matching "rejects a listing missing src/lib.rs" "$listing" "src/lib.rs"

listing="$(write_package_listing missing-main-rs Cargo.toml Cargo.lock LICENSE.md README.md src/lib.rs src/bin/shim/main.rs)"
assert_package_list_rejects_matching "rejects a listing missing the goetia bin source (src/main.rs)" "$listing" "src/main.rs"

listing="$(write_package_listing missing-shim-main-rs Cargo.toml Cargo.lock LICENSE.md README.md src/lib.rs src/main.rs)"
assert_package_list_rejects_matching "rejects a listing missing the goetia-shim bin source (src/bin/shim/main.rs)" "$listing" "src/bin/shim/main.rs"

# An empty (or otherwise truncated) listing must be caught by the positive
# control, never mistaken for "nothing leaked".
listing="$(write_package_listing empty)"
assert_package_list_rejects_matching "rejects an empty listing" "$listing" "Cargo.toml"

# Negative check: one leaked path per excluded prefix in goetia's exclude
# list (Cargo.toml's `exclude`, Task 2 step 3) — directory prefixes and
# single-file entries alike.
listing="$(write_package_listing leaks-github "${complete_listing_lines[@]}" .github/workflows/ci.yaml)"
assert_package_list_rejects_matching "rejects a leaked .github/ path" "$listing" ".github/workflows/ci.yaml"

listing="$(write_package_listing leaks-claude "${complete_listing_lines[@]}" .claude/settings.json)"
assert_package_list_rejects_matching "rejects a leaked .claude/ path" "$listing" ".claude/settings.json"

listing="$(write_package_listing leaks-scripts "${complete_listing_lines[@]}" scripts/format-section-comments.py)"
assert_package_list_rejects_matching "rejects a leaked scripts/ path" "$listing" "scripts/format-section-comments.py"

listing="$(write_package_listing leaks-prek-toml "${complete_listing_lines[@]}" prek.toml)"
assert_package_list_rejects_matching "rejects a leaked prek.toml" "$listing" "prek.toml"

listing="$(write_package_listing leaks-editorconfig "${complete_listing_lines[@]}" .editorconfig)"
assert_package_list_rejects_matching "rejects a leaked .editorconfig" "$listing" ".editorconfig"

listing="$(write_package_listing leaks-gitattributes "${complete_listing_lines[@]}" .gitattributes)"
assert_package_list_rejects_matching "rejects a leaked .gitattributes" "$listing" ".gitattributes"

listing="$(write_package_listing leaks-rustfmt-toml "${complete_listing_lines[@]}" .rustfmt.toml)"
assert_package_list_rejects_matching "rejects a leaked .rustfmt.toml" "$listing" ".rustfmt.toml"

assert_package_list_rejects_matching "rejects a listing file that does not exist" \
    "${pkg_list_root}/nonexistent" "not found"

if bash "$assert_package_list" >/dev/null 2>&1; then
    echo "FAIL - rejects being called with no arguments at all (expected rejection, got success)"
    failures=$((failures + 1))
else
    echo "ok   - rejects being called with no arguments at all"
fi

# grep itself failing (not "found" / "not found", but an actual error) must
# be distinguished from a clean exclusion result — a stub that answers -E
# calls with an error and otherwise defers to the real grep exercises that
# third branch without needing to break grep systemwide.
real_grep="$(command -v grep)"
cat > "${stub_dir}/grep" <<EOF
#!/usr/bin/env bash
if [[ "\$1" == "-E" ]]; then
    exit 2
fi
exec "${real_grep}" "\$@"
EOF
chmod +x "${stub_dir}/grep"

listing="$(write_package_listing grep-error "${complete_listing_lines[@]}")"
output="$(PATH="${stub_dir}:${PATH}" bash "$assert_package_list" "$listing" 2>&1)" && grep_error_status=0 || grep_error_status=$?
if [[ "$grep_error_status" -ne 0 && "$output" == *"exclusion unverified"* ]]; then
    echo "ok   - treats a grep failure as unverified, not clean"
else
    echo "FAIL - treats a grep failure as unverified, not clean (status ${grep_error_status}, output: ${output})"
    failures=$((failures + 1))
fi

# assert-asset-count.sh ------------------------------------------------------

assert_asset_count_ok() {
    local description="$1" expected_count="$2"
    shift 2
    if bash "$assert_asset_count" "$expected_count" "$@" >/dev/null 2>&1; then
        echo "ok   - ${description}"
    else
        echo "FAIL - ${description} (expected success, got failure)"
        failures=$((failures + 1))
    fi
}

assert_asset_count_rejects_matching() {
    local description="$1" expected_count="$2" message="$3"
    shift 3
    local output
    if output="$(bash "$assert_asset_count" "$expected_count" "$@" 2>&1)"; then
        echo "FAIL - ${description} (expected rejection, got success)"
        failures=$((failures + 1))
    elif [[ "$output" == *"$message"* ]]; then
        echo "ok   - ${description}"
    else
        echo "FAIL - ${description} (expected message containing '${message}', got: ${output})"
        failures=$((failures + 1))
    fi
}

seven_assets=()
for i in 1 2 3 4 5 6 7; do
    f="${asset_count_root}/asset-${i}"
    : > "$f"
    seven_assets+=("$f")
done

assert_asset_count_ok "accepts exactly the expected count" 7 "${seven_assets[@]}"

assert_asset_count_rejects_matching "rejects too few assets" 7 "expected 7 asset(s), got 6" \
    "${seven_assets[@]:0:6}"

# The exact vacuous-guard shape this script exists to close off: an empty
# `nullglob` expansion must never read as "0 assets to check, trivially
# satisfied".
assert_asset_count_rejects_matching "rejects a zero count" 0 "positive integer"
assert_asset_count_rejects_matching "rejects a non-numeric count" abc "positive integer"

assert_asset_count_rejects_matching "rejects a named asset that does not exist" 1 "asset not found" \
    "${asset_count_root}/does-not-exist"

if bash "$assert_asset_count" >/dev/null 2>&1; then
    echo "FAIL - rejects being called with no arguments at all (expected rejection, got success)"
    failures=$((failures + 1))
else
    echo "ok   - rejects being called with no arguments at all"
fi

# assert-test-binaries.sh -----------------------------------------------------
#
# The step this guards (`ci.yaml`'s "Run tests, elevated") drives a
# `while IFS= read -r bin; do ...; done < test-binaries.txt` loop: an empty
# file makes that loop iterate zero times and exit 0, having run nothing.
# This is the release-gate promotion of the same vacuous-guard shape
# `assert-archives.sh 0` was fixed for.

assert_test_binaries_ok() {
    local description="$1" listing="$2"
    if bash "$assert_test_binaries" "$listing" >/dev/null 2>&1; then
        echo "ok   - ${description}"
    else
        echo "FAIL - ${description} (expected success, got failure)"
        failures=$((failures + 1))
    fi
}

assert_test_binaries_rejects_matching() {
    local description="$1" listing="$2" message="$3"
    local output
    if output="$(bash "$assert_test_binaries" "$listing" 2>&1)"; then
        echo "FAIL - ${description} (expected rejection, got success)"
        failures=$((failures + 1))
    elif [[ "$output" == *"$message"* ]]; then
        echo "ok   - ${description}"
    else
        echo "FAIL - ${description} (expected message containing '${message}', got: ${output})"
        failures=$((failures + 1))
    fi
}

listing="${test_binaries_root}/one-binary"
printf '/path/to/some-test-bin\n' > "$listing"
assert_test_binaries_ok "accepts a listing with one binary" "$listing"

listing="${test_binaries_root}/multiple-binaries"
printf '/path/to/bin-a\n/path/to/bin-b\n' > "$listing"
assert_test_binaries_ok "accepts a listing with multiple binaries" "$listing"

# CRLF is how the workflow's own filter emits this file on Windows (Python's
# text-mode stdout), and the loop strips it — this script must count the
# line as present, not blank.
listing="${test_binaries_root}/crlf-binary"
printf '/path/to/bin\r\n' > "$listing"
assert_test_binaries_ok "accepts a CRLF-terminated listing" "$listing"

listing="${test_binaries_root}/truly-empty"
: > "$listing"
assert_test_binaries_rejects_matching "rejects a completely empty listing" "$listing" "no test binaries found"

# The exact reproduction from the hunt: a listing that is non-empty in bytes
# (blank lines, or CRLF-only lines) but names zero binaries once the
# workflow's own stripping rules are applied.
listing="${test_binaries_root}/blank-lines-only"
printf '\n\n' > "$listing"
assert_test_binaries_rejects_matching "rejects a listing of blank lines only" "$listing" "no test binaries found"

listing="${test_binaries_root}/crlf-blank-only"
printf '\r\n\r\n' > "$listing"
assert_test_binaries_rejects_matching "rejects a listing of CRLF-blank lines only" "$listing" "no test binaries found"

assert_test_binaries_rejects_matching "rejects a listing file that does not exist" \
    "${test_binaries_root}/nonexistent" "not found"

if bash "$assert_test_binaries" >/dev/null 2>&1; then
    echo "FAIL - rejects being called with no arguments at all (expected rejection, got success)"
    failures=$((failures + 1))
else
    echo "ok   - rejects being called with no arguments at all"
fi

# --------------------------------------------------------------------------

if [[ "$failures" -gt 0 ]]; then
    echo "${failures} test(s) failed"
    exit 1
fi
echo "all tests passed"
