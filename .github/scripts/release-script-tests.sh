#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
normalize="${script_dir}/normalize-version.sh"
assert_tag="${script_dir}/assert-tag-absent.sh"
assert_archives="${script_dir}/assert-archives.sh"
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
# all three branches are exercised without touching a real remote.

stub_dir="$(mktemp -d)"
archive_root="$(mktemp -d)"
trap 'rm -rf "$stub_dir" "$archive_root"' EXIT

make_git_stub() {
    local exit_code="$1"
    cat > "${stub_dir}/git" <<EOF
#!/usr/bin/env bash
exit ${exit_code}
EOF
    chmod +x "${stub_dir}/git"
}

assert_tag_result() {
    local description="$1" git_exit="$2" want_success="$3"
    make_git_stub "$git_exit"
    if PATH="${stub_dir}:${PATH}" bash "$assert_tag" v0.1.0 >/dev/null 2>&1; then
        [[ "$want_success" == "yes" ]] && echo "ok   - ${description}" && return 0
    else
        [[ "$want_success" == "no" ]] && echo "ok   - ${description}" && return 0
    fi
    echo "FAIL - ${description} (git exit ${git_exit}, wanted success=${want_success})"
    failures=$((failures + 1))
}

assert_tag_result "tag present (git exit 0) is refused"        0   no
assert_tag_result "tag absent (git exit 2) is accepted"        2   yes
assert_tag_result "transient failure (git exit 1) is refused"  1   no
assert_tag_result "auth failure (git exit 128) is refused"     128 no

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

# --------------------------------------------------------------------------

if [[ "$failures" -gt 0 ]]; then
    echo "${failures} test(s) failed"
    exit 1
fi
echo "all tests passed"
