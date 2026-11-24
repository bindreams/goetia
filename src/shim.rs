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

/// What a program that hosts the shim calls **first** in its own `main`, before it parses its arguments or does
/// anything else, whether or not it then calls [`main`]. On Windows a service whose stop asks the daemon to exit
/// (every `send-signal` but `kill` and `wm-close`) does so through a copy of the host program that cosca starts as a
/// console-event proxy; the copy runs the host's `main` from the top, and this call is what turns it into the proxy
/// and ends it. A host that did not make the call is refused when the service starts, with an error that names it.
/// A host that makes it late, after code of its own, runs that code in every copy. This is `cosca::init_helpers`,
/// from the cosca goetia is built on, so the host needs no cosca of its own. It does nothing on other platforms,
/// and returns at once outside a copy.
pub fn init_host() {
    #[cfg(windows)]
    cosca::init_helpers();
}

/// The shim's entry point: never returns. On Windows it runs as a service host and hands control to
/// SCM, unless the process's arguments end in `--goetia-task <id> <token>`: Task Scheduler then started it as
/// the action of goetia's task `<id>`, and it runs that task's program and records how the run ended.
/// A program that calls this from its own `main` must let those three trailing arguments through
/// its own argument parser, and leave them last, and must have called [`init_host`] first in its `main`. On every
/// other platform it prints that the shim is Windows-only and exits 1.
pub fn main() -> ! {
    #[cfg(windows)]
    crate::backend::scm::shim::run::run();

    #[cfg(not(windows))]
    {
        eprintln!("goetia-shim is Windows-only");
        std::process::exit(1);
    }
}

/// Expands to a `fn main` that runs the stock shim, [`init_host`] first: `goetia::stock_shim!();` is a whole
/// shim binary.
#[macro_export]
macro_rules! stock_shim {
    () => {
        fn main() -> ! {
            $crate::shim::init_host();
            $crate::shim::main()
        }
    };
}

/// The stock shim's bytes for `EmbeddedStockShim::new`: an
/// `Option<&'static [u8]>` that is `Some(include_bytes!(..))` of the file `GOETIA_SHIM_EXE` names
/// when the crate it expands in has its own `goetia-shim` feature on, and `None` otherwise (also
/// when that crate never declared the feature, without an `unexpected_cfgs` warning).
///
/// Expand it in the binary crate, never in a library the shim binary links, with
/// `GOETIA_SHIM_EXE` the absolute path of the built stock shim (`include_bytes!` would resolve a
/// relative one against the source file). With the feature on and no variable the build fails.
#[macro_export]
macro_rules! embed_shim {
    () => {{
        // The cfgs name the caller's feature, which rustc's check-cfg does not know in a caller that
        // never declared it.
        #[allow(unexpected_cfgs)]
        let bytes = {
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
        };
        bytes
    }};
}
