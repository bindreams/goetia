fn main() {
    // Without this, cargo reruns the script, and rebuilds the crate, after a change to any file in the package.
    println!("cargo:rerun-if-changed=build.rs");
    libsearch_guard::bins();
}
