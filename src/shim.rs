//! The shim host: what an embedding program configures and runs.

#[cfg(windows)]
pub use crate::backend::scm::shim::acl::{CheckPermissions, FolderCheck};

/// The shim's entry point: hands control to SCM and never returns. On every other platform it
/// prints that the shim is Windows-only and exits 1.
pub fn main() -> ! {
    #[cfg(windows)]
    crate::backend::scm::shim::run::run();

    #[cfg(not(windows))]
    {
        eprintln!("goetia-shim is Windows-only");
        std::process::exit(1);
    }
}

/// Expands to a `fn main` that runs the stock shim: `goetia::stock_shim!();` is a whole shim
/// binary.
#[macro_export]
macro_rules! stock_shim {
    () => {
        fn main() -> ! {
            $crate::shim::main()
        }
    };
}
