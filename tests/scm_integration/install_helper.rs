//! A one-shot child-process helper: performs exactly one
//! [`goetia::manager::ServiceManager::install`] call for a real (non-`LocalSystem`) account
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
//! `<exe> --goetia-install-as <id> <account-name> [<sid-string>]`
//!
//! With a trailing `<sid-string>`, installs `user: {id: <sid-string>}`
//! (`User::Id(AccountId::Sid)`) instead of `user: {name: <account-name>}` —
//! `deleted_account_makes_the_service_oursunusable` needs this: a
//! `User::Name` account never fails to re-resolve (no lookup at all), so
//! only the SID path can actually exercise `Ownership::OursUnusable`.
//!
//! `<exe> --goetia-task-as <verb> <id> <account-name> <shim> <command>...`
//!
//! The same for the verbs of a task of a local user that read the password: `verb` is `install` (a task
//! that runs `<command>` on `tasks::SCHEDULE`, through the shim at `<shim>`), `enable` or `disable`.
//! `uninstall` needs no password, so a test makes it in its own process.

use goetia::backend::scm::manager::Scm;
use goetia::manager::ServiceManager as _;
use goetia::spec::{AccountId, DaemonFields, DaemonSpec, Id, ServiceFields, ServiceSpec, User};

use crate::support::windows_tasks::task_spec;
use crate::tasks::SCHEDULE;
use crate::{common, support};

pub const INSTALL_AS: &str = "--goetia-install-as";
pub const TASK_AS: &str = "--goetia-task-as";

pub fn run_if_requested() -> bool {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some(INSTALL_AS) => install_service_as(&args),
        Some(TASK_AS) => task_verb_as(&args[2..]),
        _ => false,
    }
}

/// Reports a verb's result on stdout, as `OK <result>` or `ERR <error>`, and ends the process with it.
fn finish<T: std::fmt::Debug>(result: goetia::Result<T>) -> ! {
    match result {
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

fn task_verb_as(args: &[String]) -> ! {
    let [verb, id, account, shim, command @ ..] = args else {
        panic!("`{TASK_AS}` takes a verb, an id, an account, a shim and a command");
    };
    let id = Id::try_from(id.as_str()).expect("an id");
    let mgr = support::shim_manager_at(shim.into());
    match verb.as_str() {
        "install" => {
            // PROBE: a SID string is written as `user: {id: ...}`.
            let user = if account.starts_with("S-1-") {
                User::Id(AccountId::Sid(account.clone()))
            } else {
                User::Name(account.clone())
            };
            let spec = task_spec(&id, command.to_vec(), Some(SCHEDULE), user, None);
            finish(mgr.install(&spec, false))
        }
        "enable" => finish(mgr.enable(&id)),
        "disable" => finish(mgr.disable(&id)),
        other => panic!("`{TASK_AS}` has no verb `{other}`"),
    }
}

fn install_service_as(args: &[String]) -> ! {
    let id = args[2].clone();
    let account = args[3].clone();
    let user = match args.get(4) {
        Some(sid) => User::Id(AccountId::Sid(sid.clone())),
        None => User::Name(account),
    };
    let spec = ServiceSpec::new({
        let base = support::base_fields(&id, common::fixture_command(&id, 1, 1, "plain"));
        ServiceFields {
            common: DaemonFields { user, ..base.common },
            ..base
        }
    })
    .map(DaemonSpec::from)
    .unwrap();

    finish(Scm::new().install(&spec, false))
}
