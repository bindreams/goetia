#!/usr/bin/env bash
# consumer-build-tests.sh
#
# Windows CI only; the runner is Administrator and the round trips write `%ProgramFiles%\Goetia`
# and register services. Proves that a library consumer of the *packaged* goetia crate can ship
# the stock shim, and that goetia's own build routes do too:
#
#   1. a consumer built without the `goetia-shim` feature type-checks with no variable set, and
#      its `install` reports "built without a stock shim"; one that never declared the feature
#      type-checks with `unexpected_cfgs` denied;
#   2. the same consumer built in two stages (shim first, then the program with the feature and
#      `GOETIA_SHIM_EXE`) installs, runs and uninstalls a `type: simple` service, leaving no shim;
#   3. goetia's own two-stage build does the same;
#   4. `cargo install` goetia previews the shim installed beside it (the sibling route).
#
# The consumer's lockfile is resolved once online and pins one dependency below goetia's own
# lock, so it differs from goetia's. Everything after that builds offline.
set -Eeuxo pipefail
trap 'echo "failed: line $LINENO, exit $?" >&2' ERR
exec 2>&1

: "${RUNNER_TEMP:?RUNNER_TEMP must name a scratch directory}"

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
work="$(cygpath -u "$RUNNER_TEMP")/consumer-build"
shim_dir="$(cygpath -u "$PROGRAMFILES")/Goetia"

# The dependency the consumer pins below goetia's lock, and the version it pins.
pinned_crate=memchr
pinned_version=2.8.2

# Pairs of (goetia executable, service id) that a round trip may have left installed.
installed=()

cleanup() {
    local i
    for ((i = 0; i < ${#installed[@]}; i += 2)); do
        "${installed[i]}" daemon uninstall "${installed[i + 1]}" || echo "warning: could not uninstall ${installed[i + 1]}" >&2
    done
}
trap cleanup EXIT

fail() {
    echo "error: $*" >&2
    exit 1
}

# `cargo` and `rustc` take the Windows spelling of a path.
winpath() {
    cygpath -m "$1"
}

sha256_of() {
    local digest
    digest="$(sha256sum "$1")"
    echo "${digest%% *}"
}

write_manifest() {
    local dir="$1" id="$2"
    mkdir -p "$dir"
    cat > "$dir/goetia.yaml" <<EOF
id: $id
command: ['C:/Windows/System32/ping.exe', '-t', '127.0.0.1']
type: simple
EOF
}

assert_no_shim() {
    if compgen -G "$shim_dir/goetia-shim-*.exe" > /dev/null; then
        ls -l "$shim_dir"
        fail "a goetia-shim-*.exe remains under $shim_dir"
    fi
}

# `daemon install --dry-run` names the shim by the hash of the file it was built from.
assert_previews_shim() {
    local exe="$1" dir="$2" shim="$3" out hex
    out="$("$exe" daemon install --dry-run -f "$dir")"
    hex="$(sha256_of "$shim")"
    if [[ "$out" != *"Program Files\\\\Goetia\\\\goetia-shim-$hex.exe"* ]]; then
        echo "$out"
        fail "the dry run does not name goetia-shim-$hex.exe under Program Files\\Goetia"
    fi
}

# Install, run, uninstall a `type: simple` service, and expect nothing of the shim to remain.
round_trip() {
    local exe="$1" id="$2" dir="$3" status
    write_manifest "$dir" "$id"
    assert_no_shim

    installed+=("$exe" "$id")
    "$exe" daemon install -f "$dir" --start
    status="$("$exe" daemon status "$id")"
    echo "$status"
    if [[ "$status" != *"$id: running"* ]]; then
        fail "$id is not running"
    fi
    "$exe" daemon uninstall "$id"
    assert_no_shim
}

write_consumer() {
    local dir="$1" crate_dir="$2" version="$3"
    mkdir -p "$dir/src/bin"

    cat > "$dir/Cargo.toml" <<EOF
[package]
name = "goetia-consumer"
version = "0.0.0"
edition = "2024"
publish = false

[features]
goetia-shim = []

[dependencies]
goetia = "=$version"
clap = "4"
$pinned_crate = "=$pinned_version"

[patch.crates-io]
goetia = { path = "$crate_dir" }
EOF

    echo 'goetia::stock_shim!();' > "$dir/src/bin/goetia-shim.rs"

    # goetia's `src/main.rs`, with the provider fixed to the embedded one: a consumer has no
    # sibling to fall back on.
    cat > "$dir/src/main.rs" <<'EOF'
use std::io;

use clap::Parser as _;
use goetia::shim::{EmbeddedStockShim, ShimProvider as _};

fn stock_shim() -> impl goetia::shim::ShimProvider {
    EmbeddedStockShim::new(goetia::embed_shim!(), None)
}

fn main() {
    let cli = goetia::cli::Cli::parse();

    let get_manager = || goetia::cli::manager(stock_shim());
    let is_elevated = goetia::cli::is_elevated;
    let host = || stock_shim().preview();

    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let code = goetia::cli::dispatch(&cli, &get_manager, &is_elevated, &host, &mut stdout, &mut stderr);
    std::process::exit(code);
}
EOF
}

# A lockfile that differs from goetia's, or the consumer does not exercise a lockfile of its own.
assert_lock_differs() {
    local consumer_lock="$1" own pinned
    own="$(grep -A1 "^name = \"$pinned_crate\"\$" "$repo/Cargo.lock" | grep '^version')"
    pinned="$(grep -A1 "^name = \"$pinned_crate\"\$" "$consumer_lock" | grep '^version')"
    if [[ "$own" == "$pinned" ]]; then
        fail "the consumer locks $pinned_crate as goetia does ($own)"
    fi
}

# `goetia::embed_shim!` names the expanding crate's own `goetia-shim` feature. `cargo rustc` passes
# the flag to the consumer's crate alone, so step (1)'s artifacts for the dependencies are reused
# (`RUSTFLAGS` would rebuild every one of them).
assert_undeclared_feature_is_quiet() {
    local consumer="$1" dir="$work/undeclared-consumer"
    mkdir -p "$dir/src"
    sed -e '/^\[features\]$/d' -e '/^goetia-shim = \[\]$/d' "$consumer/Cargo.toml" > "$dir/Cargo.toml"
    cp "$consumer/Cargo.lock" "$dir/Cargo.lock"
    cat > "$dir/src/main.rs" <<'EOF'
fn main() {
    let _: Option<&'static [u8]> = goetia::embed_shim!();
}
EOF
    local target_dir
    target_dir="$(winpath "$consumer/target")"
    if ! (
        cd "$dir" &&
            env -u GOETIA_SHIM_EXE cargo rustc --locked --profile check --target-dir "$target_dir" \
                -- -D unexpected_cfgs
    ); then
        fail "a consumer that never declared the feature failed 'cargo rustc -D unexpected_cfgs' (see the output above)"
    fi
}

consumer_routes() {
    local crate_file crate_name version crate_dir consumer dir shim target
    consumer="$work/consumer"
    target="$consumer/target/debug"
    dir="$work/manifest-consumer"

    cd "$repo"
    cargo package --locked
    crate_file="$(ls "$repo"/target/package/goetia-*.crate)"
    crate_name="$(basename "$crate_file" .crate)"
    version="${crate_name#goetia-}"
    mkdir -p "$work/crate"
    tar -xzf "$crate_file" -C "$work/crate"
    crate_dir="$work/crate/$crate_name"

    write_consumer "$consumer" "$(winpath "$crate_dir")" "$version"
    cd "$consumer"
    cargo generate-lockfile
    cargo fetch --locked
    assert_lock_differs "$consumer/Cargo.lock"

    # Offline from here on.
    export CARGO_NET_OFFLINE=true

    # (1) No variable, no feature: the consumer type-checks.
    env -u GOETIA_SHIM_EXE cargo check --locked

    # (1b) The same, in a consumer that never declared the feature: no warning.
    assert_undeclared_feature_is_quiet "$consumer"

    # (2) No feature: the program builds, and install says why it has no shim.
    write_manifest "$dir" goetia-ci-consumer
    env -u GOETIA_SHIM_EXE cargo build --bin goetia-consumer --locked
    local out
    if out="$("$target/goetia-consumer.exe" daemon install --dry-run -f "$dir" 2>&1)"; then
        echo "$out"
        fail "a consumer built without the feature previewed a stock shim"
    fi
    if [[ "$out" != *"built without a stock shim"* ]]; then
        echo "$out"
        fail "the dry run did not report 'built without a stock shim'"
    fi

    # (3) Two stages: the shim, then the program with the feature and the shim's absolute path.
    cargo build --bin goetia-shim --locked
    shim="$target/goetia-shim.exe"
    GOETIA_SHIM_EXE="$(winpath "$shim")" cargo build --bin goetia-consumer --features goetia-shim --locked
    assert_previews_shim "$target/goetia-consumer.exe" "$dir" "$shim"
    round_trip "$target/goetia-consumer.exe" goetia-ci-consumer "$dir"
}

goetia_routes() {
    local dir shim target install_root
    dir="$work/manifest-goetia"
    target="$repo/target/debug"
    install_root="$work/inst"

    cd "$repo"

    # The two-stage build.
    cargo build --bin goetia-shim --locked
    shim="$target/goetia-shim.exe"
    GOETIA_SHIM_EXE="$(winpath "$shim")" cargo build --bin goetia --features goetia-shim --locked
    write_manifest "$dir" goetia-ci-two-stage
    assert_previews_shim "$target/goetia.exe" "$dir" "$shim"
    round_trip "$target/goetia.exe" goetia-ci-two-stage "$dir"

    # `cargo install`: the shim is the sibling of the program.
    cargo install --path . --root "$(winpath "$install_root")" --locked --offline
    assert_previews_shim "$install_root/bin/goetia.exe" "$dir" "$install_root/bin/goetia-shim.exe"
}

mkdir -p "$work"

# goetia's own dependencies, fetched while the network is still on: `consumer_routes` goes offline
# and the setting persists into `goetia_routes`.
cd "$repo"
cargo fetch --locked

consumer_routes
goetia_routes
