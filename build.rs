fn main() {
    // Without this, cargo reruns the script, and rebuilds the crate, after a change to any file in the package.
    println!("cargo:rerun-if-changed=build.rs");
    // Also runs, compiling the unused initializer, when goetia is only a library dependency: a build script
    // cannot tell whether its package's bins are built.
    libsearch_guard::bins();
}
