// In test mode the bin's `main` becomes a skuld test runner; the regular
// CLI dispatch below is dead code in that build.
#![cfg_attr(test, allow(dead_code))]

#[cfg(not(test))]
fn main() {
    use std::io;

    use clap::Parser as _;

    let cli = goetia::cli::Cli::parse();

    // Both lazy: `dispatch` only calls `native()`/`is_elevated` when the
    // dispatched subcommand actually needs a manager or elevation — see
    // `goetia::cli`'s module doc comment for why (`install --dry-run` and
    // `show`/`diff -f` need neither).
    let get_manager = || goetia::manager::native();
    let is_elevated = goetia::cli::is_elevated;

    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let code = goetia::cli::dispatch(&cli, &get_manager, &is_elevated, &mut stdout, &mut stderr);
    std::process::exit(code);
}

#[cfg(test)]
fn main() {
    skuld::run_all();
}
