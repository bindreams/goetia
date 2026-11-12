//! The built `goetia` and `goetia-shim` do not search their own folder for libraries.
//!
//! `libsearch_guard::verify` reads the built files. On Windows it checks the effects of `build.rs`'s
//! `libsearch_guard::bins()` in the PE file (System32-only import loading, no `vcruntime140.dll` import), not
//! its pre-`main` initializer. On Linux and macOS it checks that the binary carries no RPATH/RUNPATH or
//! `@rpath` search relative to itself.

fn main() {
    goetia::testing::run_all();
}

// The `goetia` bin requires `cli`; the shim is built without it.
#[cfg(feature = "cli")]
#[skuld::test]
fn goetia_does_not_search_its_own_folder() {
    libsearch_guard::verify(env!("CARGO_BIN_EXE_goetia")).unwrap();
}

#[skuld::test]
fn goetia_shim_does_not_search_its_own_folder() {
    libsearch_guard::verify(env!("CARGO_BIN_EXE_goetia-shim")).unwrap();
}
