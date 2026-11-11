#!/usr/bin/env bash
# Checks the doc links `cargo doc` never builds: the library's `cfg(test)` modules and every
# `[[test]]` target. Extra arguments go to each rustdoc call.
set -euo pipefail
cd "$(dirname "$0")/../.."

# `cli` is a default feature; the test modules also need `testing`. `--all-features` would need a
# built shim (`goetia-shim`).
cargo rustdoc --locked --lib --features testing -- --cfg test --document-private-items "$@"

# `cargo rustdoc` sets no `CARGO_BIN_EXE_*` for a test target, and the tests read them with `env!`
# at compile time; documenting never runs them, so any value builds.
targets=$(sed -n '/^\[\[test\]\]/{n;s/^name = "\(.*\)"$/\1/p;}' Cargo.toml)
[[ -n "$targets" ]] || { echo "rustdoc-tests.sh: no [[test]] targets found in Cargo.toml" >&2; exit 1; }
for target in $targets; do
    env "CARGO_BIN_EXE_goetia=unused" "CARGO_BIN_EXE_goetia-shim=unused" \
        cargo rustdoc --locked --test "$target" --features testing -- --document-private-items "$@"
done
