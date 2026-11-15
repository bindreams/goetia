//! `verify-hardening <executable>...`: checks each executable with `libsearch_guard::verify`, reports every one
//! that fails, and exits 1 if any did. No arguments: usage on stderr, exit 2.

use std::process::ExitCode;

fn main() -> ExitCode {
    let paths: Vec<_> = std::env::args_os().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: verify-hardening <executable>...");
        return ExitCode::from(2);
    }

    let mut failed = false;
    for path in &paths {
        match libsearch_guard::verify(path) {
            Ok(()) => println!("hardened: {}", path.display()),
            Err(err) => {
                eprintln!("not hardened: {}: {err}", path.display());
                failed = true;
            }
        }
    }
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
