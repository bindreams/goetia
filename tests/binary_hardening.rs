//! The built `goetia` and `goetia-shim` do not search their own folder for libraries.
//!
//! `libsearch_guard::verify` reads the built files: on Windows it checks that `build.rs`'s
//! `libsearch_guard::bins()` landed (System32-only loading, no `vcruntime140.dll` import); on Linux and
//! macOS, that the binary carries no RPATH/RUNPATH or `@rpath` search relative to itself.

fn main() {
    goetia::testing::run_all();
}

#[skuld::test]
fn goetia_does_not_search_its_own_folder() {
    libsearch_guard::verify(env!("CARGO_BIN_EXE_goetia")).unwrap();
}

#[skuld::test]
fn goetia_shim_does_not_search_its_own_folder() {
    libsearch_guard::verify(env!("CARGO_BIN_EXE_goetia-shim")).unwrap();
}
