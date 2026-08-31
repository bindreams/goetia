//! A one-shot child-process helper: performs exactly one
//! [`ServiceManager::install`] call for a real (non-`LocalSystem`) account
//! and exits.
//!
//! `real_account_gets_service_logon_right` needs `GOETIA_SERVICE_PASSWORD`
//! set for that one `install` call only. `std::env::set_var` on the test
//! binary's own process is process-wide mutable state shared with every
//! other `#[skuld::test]` that may be running concurrently in another
//! thread — exactly the data race this project's rules forbid relying on,
//! "even if the correct outcome is 99.99% likely". Spawning this helper
//! with `Command::env` instead scopes the variable to a process nobody else
//! shares.
//!
//! `<exe> --goetia-install-as <id> <account-name>`

use goetia::backend::scm::manager::ScmManager;
use goetia::manager::ServiceManager as _;
use goetia::spec::User;

use crate::common;

pub const INSTALL_AS: &str = "--goetia-install-as";

pub fn run_if_requested() -> bool {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some(INSTALL_AS) {
        return false;
    }
    let id = args[2].clone();
    let account = args[3].clone();
    let spec = common::mk_spec_as(
        &id,
        common::fixture_command(&id, 1, 1, "plain"),
        Default::default(),
        User::Name(account),
    );

    match ScmManager::new().install(&spec, false) {
        Ok(outcome) => {
            println!("OK {outcome:?}");
            std::process::exit(0);
        }
        Err(e) => {
            println!("ERR {e}");
            std::process::exit(1);
        }
    }
}
