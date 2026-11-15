//! Runs `libsearch_guard::verify` on each path given, for binaries that `cargo test` does not see (the
//! release build, the two-stage `goetia.exe`). Exits 1 if any has a violation.

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    assert!(!paths.is_empty(), "usage: verify <executable>...");
    let mut failed = false;
    for path in &paths {
        match libsearch_guard::verify(path) {
            Ok(()) => println!("hardened: {path}"),
            Err(err) => {
                eprintln!("not hardened: {path}: {err}");
                failed = true;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}
