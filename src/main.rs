// In test mode the bin's `main` becomes a skuld test runner; the regular
// CLI dispatch below is dead code in that build.
#![cfg_attr(test, allow(dead_code))]

#[cfg(not(test))]
fn main() {
    println!("goetia {}", goetia::version());
}

#[cfg(test)]
fn main() {
    skuld::run_all();
}
