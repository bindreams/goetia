//! A one-shot child process that installs one `protocol: plain` service for a real local account and
//! exits: `GOETIA_SERVICE_PASSWORD` is read from the environment of the process that installs, and
//! `set_var` in the test binary would race every test running beside it. Spawning this with
//! `Command::env` scopes the variable to a process nobody else shares (see
//! `tests/scm_integration/install_helper.rs`, which does the same for `protocol: native`).
//!
//! `<exe> --goetia-shim-install-as <id> <account> <daemon-exe> <log-path> <port>`
//!
//! Installs the `report <port>` fixture, run from `<daemon-exe>` as `.\<account>` and logging to
//! `<log-path>`, through the stock shim placed in `%ProgramFiles%\Goetia` with the folder check on.

use std::path::PathBuf;

use goetia::manager::ServiceManager as _;
use goetia::shim::CheckPermissions;
use goetia::spec::{DaemonFields, DaemonSpec, Protocol, ServiceFields, ServiceSpec, User};

use crate::common::{embedded_manager, fixture_command_with_exe};
use crate::support;

pub const INSTALL_AS: &str = "--goetia-shim-install-as";

pub fn run_if_requested() -> bool {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some(INSTALL_AS) {
        return false;
    }
    let [_, _, id, account, exe, log, port] = &args[..] else {
        eprintln!("usage: {INSTALL_AS} <id> <account> <daemon-exe> <log-path> <port>");
        std::process::exit(2);
    };
    let spec = ServiceSpec::new({
        let base = support::base_fields(id, fixture_command_with_exe(exe, "report", &[port]));
        ServiceFields {
            common: DaemonFields {
                user: User::Name(format!(r".\{account}")),
                logs: Some(PathBuf::from(log)),
                ..base.common
            },
            protocol: Protocol::Plain,
            ..base
        }
    })
    .map(DaemonSpec::from)
    .unwrap_or_else(|e| panic!("the helper's spec: {}", e.report()));
    match embedded_manager(None, CheckPermissions::Enable).install(&spec, false) {
        Ok(outcome) => {
            println!("OK {outcome:?}");
            std::process::exit(0);
        }
        Err(e) => {
            println!("ERR {}", e.report());
            std::process::exit(1);
        }
    }
}
