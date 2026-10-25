use std::path::Path;

use super::*;

fn joined(commands: &[Vec<String>]) -> Vec<String> {
    commands.iter().map(|command| command.join(" ")).collect()
}

#[skuld::test]
fn the_directory_is_closed_to_everyone_but_system_and_administrators_and_the_service_account() {
    let commands = dir_commands(Path::new(r"C:\ProgramData\Goetia\state"), Some("S-1-5-19"));

    assert_eq!(
        joined(&commands),
        [
            r"C:\ProgramData\Goetia\state /grant *S-1-5-18:(OI)(CI)F /grant *S-1-5-32-544:(OI)(CI)F /grant *S-1-5-19:(OI)(CI)M",
            r"C:\ProgramData\Goetia\state /inheritance:r",
        ]
    );
}

#[skuld::test]
fn a_local_system_service_needs_no_grant_beyond_system() {
    let dir = dir_commands(Path::new("d"), None);
    let file = file_commands(Path::new("f"), None);

    assert!(joined(&dir).iter().all(|c| !c.contains("S-1-5-19")), "{dir:?}");
    assert_eq!(
        joined(&file),
        [
            "f /reset",
            "f /grant *S-1-5-18:F /grant *S-1-5-32-544:F",
            "f /inheritance:r"
        ]
    );
}

#[skuld::test]
fn the_file_is_reset_then_granted_to_the_service_account_alone_then_cut_from_its_parent() {
    // Reset first so an account the service no longer runs as loses its entry; inheritance last, so
    // the service account never loses its access between the steps.
    let commands = file_commands(Path::new(r"C:\ProgramData\Goetia\state\frpc"), Some("S-1-5-80-1-2-3"));

    assert_eq!(
        joined(&commands),
        [
            r"C:\ProgramData\Goetia\state\frpc /reset",
            r"C:\ProgramData\Goetia\state\frpc /grant *S-1-5-18:F /grant *S-1-5-32-544:F /grant *S-1-5-80-1-2-3:M",
            r"C:\ProgramData\Goetia\state\frpc /inheritance:r",
        ]
    );
}
