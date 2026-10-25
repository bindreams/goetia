use std::path::Path;
use std::time::Duration;

use super::*;
use crate::backend::scm::generate::FailureActions;
use crate::backend::scm::state_file::{Actions, Cause, RestartDue};

fn joined(commands: &[Vec<String>]) -> Vec<String> {
    commands.iter().map(|command| command.join(" ")).collect()
}

#[skuld::test]
fn the_state_directory_is_closed_to_everyone_but_system_and_administrators() {
    // No service account can create, replace or delete anything directly under it: each service
    // has a directory of its own, and a file in it is all the account may touch.
    let commands = dir_commands(Path::new(r"C:\ProgramData\Goetia\state"));

    assert_eq!(
        joined(&commands),
        [
            r"C:\ProgramData\Goetia\state /grant *S-1-5-18:(OI)(CI)F /grant *S-1-5-32-544:(OI)(CI)F",
            r"C:\ProgramData\Goetia\state /inheritance:r",
        ]
    );
}

#[skuld::test]
fn a_service_directory_is_reset_to_the_state_directorys_grants_then_given_to_its_account_alone() {
    // The reset takes away the entry of an account the service no longer runs as, from the
    // directory and from what is in it. Everything created in it afterwards, temporary files
    // included, inherits exactly this.
    let dir = r"C:\ProgramData\Goetia\state\svc-frpc";
    let commands = service_dir_commands(Path::new(dir), Some("S-1-5-80-1-2-3"));

    assert_eq!(
        joined(&commands),
        [
            format!("{dir} /reset /T"),
            format!("{dir} /grant *S-1-5-80-1-2-3:(OI)(CI)M")
        ]
    );
}

#[skuld::test]
fn a_local_system_service_needs_no_grant_beyond_what_the_state_directory_gives() {
    let commands = service_dir_commands(Path::new("d"), None);

    assert_eq!(joined(&commands), ["d /reset /T"]);
}

#[skuld::test]
fn the_actions_recorded_for_the_shim_are_the_ones_the_service_is_registered_with() {
    let configured = configured(Some(FailureActions {
        delay: Duration::from_secs(30),
        reset_period: Duration::from_secs(86_400),
        on_non_crash_failures: true,
    }));

    assert_eq!(configured, Actions::new(true, &[Duration::from_secs(30)]));
    assert_eq!(configured.after(Cause::Reported), RestartDue::After(30_000));
}

#[skuld::test]
fn a_service_registered_without_failure_actions_is_recorded_as_restarting_nothing() {
    let configured = configured(None);

    assert_eq!(configured.after(Cause::Reported), RestartDue::Never);
    assert_eq!(configured.after(Cause::Crashed), RestartDue::Never);
}
