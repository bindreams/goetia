// In test mode the bin's `main` becomes a skuld test runner; the regular
// CLI dispatch below is dead code in that build.
#![cfg_attr(test, allow(dead_code))]

/// The stock shim goetia.exe installs `protocol: plain` services under: the one embedded in this
/// binary (a release build, `goetia-shim` feature)...
#[cfg(all(windows, feature = "goetia-shim"))]
fn stock_shim() -> impl goetia::shim::ShimProvider {
    goetia::shim::EmbeddedStockShim::new(goetia::embed_shim!(), None)
}

/// ...or, in a `cargo install` build, the `goetia-shim.exe` beside it.
#[cfg(all(windows, not(feature = "goetia-shim")))]
fn stock_shim() -> impl goetia::shim::ShimProvider {
    goetia::cli::sibling_stock_shim()
}

#[cfg(not(test))]
fn main() {
    use std::io;

    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let cli = match goetia::cli::parse(std::env::args_os(), &mut stdout, &mut stderr) {
        Ok(cli) => cli,
        Err(code) => std::process::exit(code),
    };

    // All three lazy: `dispatch` only builds a manager, checks elevation or asks for the host
    // command when the dispatched subcommand actually needs it — see `goetia::cli`'s module doc
    // comment for why (`install --dry-run` and `show`/`diff -f` need no manager and no elevation,
    // and only a Windows `install --dry-run` of a `protocol: plain` daemon needs the host command).
    #[cfg(windows)]
    let get_manager = || goetia::cli::manager(stock_shim());
    #[cfg(not(windows))]
    let get_manager = || goetia::manager::native();
    let is_elevated = goetia::cli::is_elevated;
    #[cfg(windows)]
    let host = || {
        use goetia::shim::ShimProvider as _;
        stock_shim().preview()
    };
    #[cfg(not(windows))]
    let host = || -> goetia::Result<goetia::shim::HostCommand> {
        unreachable!("only a Windows `install --dry-run` asks for the host command")
    };

    let code = goetia::cli::dispatch(&cli, &get_manager, &is_elevated, &host, &mut stdout, &mut stderr);
    std::process::exit(code);
}

#[cfg(test)]
fn main() {
    goetia::testing::run_all();
}
