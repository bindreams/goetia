//! The check of a daemon's own `logs:` path at install, for a `protocol: plain` service and for a task: both run
//! goetia's shim as the daemon's account, which creates and appends to the log. An install refuses a path that
//! other accounts can change, or whose folder is missing, or whose log is a link or has more than one name, and
//! registers nothing. The folders here are made under `%ProgramData%`, which is where the check's own ancestors
//! pass (the user's profile is a folder that user can change, so a log there is refused).

use std::path::{Path, PathBuf};

use goetia::backend::scm::manager::Scm;
use goetia::decide::Outcome;
use goetia::error::{Error, FolderRole};
use goetia::manager::ServiceManager as _;
use goetia::spec::{DaemonFields, DaemonSpec, Id, Protocol, ServiceFields, ServiceSpec, User};
use goetia::testing::ADD_ENTRIES_TO;

use crate::common::fixture_command;
use crate::managed::UNIT_DIR_EXCLUSIVE;
use crate::support::windows_tasks::task_spec;
use crate::support::{self, Group, scm_group};
use crate::tasks::TaskGuard;

/// A daemon that runs as SYSTEM and logs to `logs`: a service (`protocol` as the test says) or a task.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Service,
    Task,
}

const KINDS: [Kind; 2] = [Kind::Service, Kind::Task];

impl Kind {
    fn spec(self, id: &Id, protocol: Protocol, logs: &Path) -> DaemonSpec {
        match self {
            Kind::Service => {
                let base = support::base_fields(id.as_str(), fixture_command(id.as_str(), 1, 1, "plain"));
                ServiceSpec::new(ServiceFields {
                    common: DaemonFields {
                        logs: Some(logs.to_path_buf()),
                        ..base.common
                    },
                    protocol,
                    ..base
                })
                .map(DaemonSpec::from)
                .expect("a valid spec")
            }
            Kind::Task => task_spec(id, vec![support::current_exe_str()], None, User::Root, Some(logs)),
        }
    }
}

/// Grants every user the right to add files and folders to `folder`, as a folder made with a plain `mkdir` under
/// a shared parent often is.
fn let_users_add_to(folder: &Path) {
    let folder = folder.to_str().expect("a UTF-8 path");
    support::cmd::run("icacls", &[folder, "/grant", "*S-1-5-32-545:(OI)(CI)(WD,AD)"]).expect_ok();
}

fn assert_not_installed(mgr: &Scm, id: &Id, what: &str) {
    let absent = mgr.status(id).expect_err("nothing was installed");
    assert!(
        matches!(absent.cause(), Error::NotInstalled { .. }),
        "{what}: {absent:?}"
    );
}

/// An install whose `logs:` folder is not there is refused, and the shim makes none. Nothing is installed.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_missing_logs_folder_is_refused_at_install(#[fixture(scm_group)] _group: &Group) {
    let mgr = support::stock_shim_manager();
    for kind in KINDS {
        let guard = TaskGuard::new();
        let missing = support::program_data().join(support::random_test_id());
        let logs = missing.join("app.log");

        let refused = mgr
            .install(&kind.spec(&guard.id, Protocol::Plain, &logs), false)
            .expect_err("a folder that does not exist");

        let said = refused.report().to_string();
        assert!(said.contains("does not exist"), "{kind:?}: {said}");
        assert!(said.contains("icacls"), "{kind:?}: {said}");
        assert_not_installed(&mgr, &guard.id, &format!("{kind:?}"));
        assert!(!missing.exists(), "{kind:?}: a folder was made");
    }
}

/// A folder other accounts can add entries to is refused, on a first install and on one that would find the
/// daemon up to date: the folder changed after the first. The error names the account, the right and the
/// remedy; the daemon is not touched.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_logs_folder_others_can_add_to_is_refused_at_install_and_at_an_up_to_date_reinstall(
    #[fixture(scm_group)] _group: &Group,
) {
    let mgr = support::stock_shim_manager();
    for kind in KINDS {
        let guard = TaskGuard::new();
        let folder = support::protected_dir(&[]);
        let logs = folder.path().join("app.log");
        let spec = kind.spec(&guard.id, Protocol::Plain, &logs);

        let created = mgr
            .install(&spec, false)
            .expect("a folder only administrators can change");
        assert!(matches!(created, Outcome::Create), "{kind:?}: {created:?}");
        let_users_add_to(folder.path());
        let refused = mgr.install(&spec, false).expect_err("a folder users can add to");

        let said = refused.report().to_string();
        match refused.cause() {
            Error::FolderUnsafe {
                path,
                account,
                right,
                role: FolderRole::DaemonLog { .. },
            } => {
                assert_eq!(*right, ADD_ENTRIES_TO, "{kind:?}: {said}");
                assert!(account.ends_with("Users"), "{kind:?}: {said}");
                assert_eq!(path.file_name(), folder.path().file_name(), "{kind:?}: {said}");
            }
            other => panic!("{kind:?}: a folder users can add to must be refused for its permissions: {other:?}"),
        }
        assert!(said.contains("icacls"), "{kind:?}: {said}");
        mgr.status(&guard.id).expect("the installed daemon is as it was");

        // A first install is refused as well, and registers nothing.
        let other = TaskGuard::new();
        let first = mgr
            .install(&kind.spec(&other.id, Protocol::Plain, &logs), false)
            .expect_err("a folder users can add to");
        assert!(
            matches!(first.cause(), Error::FolderUnsafe { .. }),
            "{kind:?}: {first:?}"
        );
        assert_not_installed(&mgr, &other.id, &format!("{kind:?}"));
    }
}

/// A log that is a link, or a file with a second name, is refused.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_log_that_is_a_link_or_has_two_names_is_refused_at_install(#[fixture(scm_group)] _group: &Group) {
    let mgr = support::stock_shim_manager();
    for kind in KINDS {
        let guard = TaskGuard::new();
        let folder = support::protected_dir(&[]);
        let logs = folder.path().join("app.log");
        let spec = kind.spec(&guard.id, Protocol::Plain, &logs);

        std::fs::write(&logs, "").expect("a log");
        std::fs::hard_link(&logs, folder.path().join("second-name")).expect("a second name");
        let said = mgr.install(&spec, false).expect_err("two names").report().to_string();
        assert!(said.contains("more than one name"), "{kind:?}: {said}");

        std::fs::remove_file(folder.path().join("second-name")).expect("remove the second name");
        std::fs::remove_file(&logs).expect("remove the log");
        let elsewhere = folder.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("a folder to lead to");
        let (link, target): (PathBuf, PathBuf) = (logs.clone(), elsewhere);
        support::cmd::run(
            "cmd",
            &[
                "/C",
                "mklink",
                "/J",
                link.to_str().expect("UTF-8"),
                target.to_str().expect("UTF-8"),
            ],
        )
        .expect_ok();
        let said = mgr.install(&spec, false).expect_err("a link").report().to_string();
        assert!(said.contains("is a link"), "{kind:?}: {said}");
        assert_not_installed(&mgr, &guard.id, &format!("{kind:?}"));
    }
}

/// A `protocol: native` service has no shim, and its `logs:` is not the shim's to open: it is not judged.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_native_services_logs_is_not_judged(#[fixture(scm_group)] _group: &Group) {
    let mgr = support::stock_shim_manager();
    let guard = TaskGuard::new();
    let missing = support::program_data().join(support::random_test_id()).join("app.log");

    let created = mgr
        .install(&Kind::Service.spec(&guard.id, Protocol::Native, &missing), false)
        .expect("a native service is not judged for its logs");

    assert!(matches!(created, Outcome::Create), "{created:?}");
}

/// The folder a plain `mkdir` makes directly under the system drive is not a folder a log can be in: it
/// inherits what the drive's root grants, which on the CI image includes every authenticated user. The test
/// prints the folder's permissions, which are the measurement of the image's default, and the refusal.
#[skuld::test(labels = [UNIT_DIR_EXCLUSIVE], serial = UNIT_DIR_EXCLUSIVE)]
fn a_folder_made_with_the_images_defaults_under_the_system_drive_is_refused(#[fixture(scm_group)] _group: &Group) {
    let mgr = support::stock_shim_manager();
    let drive = std::env::var_os("SystemDrive").expect("SystemDrive is set");
    let folder = PathBuf::from(format!("{}\\{}", drive.to_string_lossy(), support::random_test_id()));
    // Registered before the folder is made.
    let guard = support::cleanup::DirGuard::new(folder.clone());
    std::fs::create_dir(&folder).unwrap_or_else(|e| panic!("create {}: {e}", folder.display()));
    let dacl = support::cmd::run("icacls", &[folder.to_str().expect("a UTF-8 path")]).expect_ok();
    println!("the permissions of a folder made under the system drive with the image's defaults:\n{dacl}");

    for kind in KINDS {
        let task = TaskGuard::new();
        let refused = mgr
            .install(
                &kind.spec(&task.id, Protocol::Plain, &guard.path().join("app.log")),
                false,
            )
            .expect_err("a folder with the image's default permissions");

        println!("{kind:?}: {}", refused.report());
        match refused.cause() {
            Error::FolderUnsafe { account, right, .. } => {
                assert!(!account.is_empty() && !right.is_empty(), "{kind:?}: {refused:?}");
            }
            other => panic!("{kind:?}: refused for its permissions, got {other:?}"),
        }
        assert_not_installed(&mgr, &task.id, &format!("{kind:?}"));
    }
}
