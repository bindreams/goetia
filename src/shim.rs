//! The shim host: what an embedding program configures and runs.

#[cfg(windows)]
pub use crate::backend::scm::shim::acl::{CheckPermissions, FolderCheck};
#[cfg(windows)]
pub use crate::backend::scm::shim::embedded::EmbeddedStockShim;
pub use crate::backend::scm::shim::provider::HostCommand;
#[cfg(windows)]
pub use crate::backend::scm::shim::provider::ShimProvider;
#[cfg(windows)]
pub use crate::backend::scm::shim::standalone::StandaloneShim;
#[cfg(windows)]
pub use crate::backend::scm::shim::sweep::SweepFailure;

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

/// The stock shim's bytes for [`EmbeddedStockShim::new`](crate::shim::EmbeddedStockShim::new): an
/// `Option<&'static [u8]>` that is `Some(include_bytes!(..))` of the file `GOETIA_SHIM_EXE` names
/// when the crate it expands in has its own `goetia-shim` feature on, and `None` otherwise.
///
/// Expand it in the binary crate, never in a library the shim binary links, with
/// `GOETIA_SHIM_EXE` the absolute path of the built stock shim (`include_bytes!` would resolve a
/// relative one against the source file). With the feature on and no variable the build fails.
#[macro_export]
macro_rules! embed_shim {
    () => {{
        #[cfg(feature = "goetia-shim")]
        {
            Some(include_bytes!(env!(
                "GOETIA_SHIM_EXE",
                "GOETIA_SHIM_EXE must be the absolute path of the built stock shim when the goetia-shim feature is on"
            )) as &'static [u8])
        }
        #[cfg(not(feature = "goetia-shim"))]
        {
            None::<&'static [u8]>
        }
    }};
}
