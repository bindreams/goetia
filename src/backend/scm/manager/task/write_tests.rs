//! The write half of the task side over the table-driven fake of Task Scheduler, with the run records
//! in a temporary directory. What the machine must hold before a task is written (its account, its
//! logon right, the event source) is the tests' own closure, so nothing here changes the machine.

use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::PathBuf;
use std::sync::Mutex;

use tempfile::TempDir;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
};

use super::*;
use crate::backend::scm::instance::InstanceId;
use crate::backend::scm::manager::task::api::{Instance, Registration};
use crate::backend::scm::manager::task::fake::{Call, Fake};
use crate::backend::scm::shim::acl::acl_tests::windows::junction;
use crate::backend::scm::shim::acl::read_object;
use crate::backend::scm::sid::{Sid, sid_from_parts, system};
use crate::backend::scm::test_support::{
    TASK_TOKEN, Undeletable, delete_pending, make_mount_point, registered_task, registered_task_with, task_host,
    task_spec, task_spec_as, task_token,
};
use crate::blob::Blob;
use crate::decide::{self, ExternalConfig, Ownership};
use crate::elevation::test_support::process_user_sid;
use crate::manager::State;
use crate::spec::{AccountId, DaemonKind, User};
use crate::testing::groups::{Group, scm_group};

fn id() -> Id {
    Id::try_from("nightly").unwrap()
}

const PATH: &str = r"\Goetia\nightly";
const ID: &str = "nightly";

fn blob(spec: &DaemonSpec) -> Blob {
    Blob {
        schema: 1,
        version: crate::version().to_string(),
        spec: spec.clone(),
    }
}

fn scm(fake: &Fake) -> Scm {
    Scm::new().with_tasks(Box::new(fake.clone()))
}

fn definition(spec: &DaemonSpec) -> TaskXml {
    TaskXml::new(spec, task_host()).unwrap()
}

/// The snapshot `read` makes of `spec`'s task as Task Scheduler holds it.
fn snapshot_of(spec: &DaemonSpec, armed: bool) -> TaskSnapshot {
    TaskSnapshot::new(id(), Ok(blob(spec)), registered_task(spec, armed))
}

/// The spec's task, and the temporary `%ProgramData%` its run records go under. Nothing is made.
fn records() -> (TempDir, Runs) {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());
    (dir, runs)
}

/// [`records`] with the task's directory and its default log made plainly, for a write that gets as far as
/// making them. Making them as an install does names Administrators as their owner, which needs an
/// elevated token (a test of the `scm` group makes them); ones that are there have only their permissions
/// set.
fn made_records() -> (TempDir, Runs) {
    let (dir, runs) = records();
    std::fs::create_dir_all(records_dir(&dir)).unwrap();
    let logs = crate::backend::scm::folder::logs_folder(dir.path());
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(default_log(&dir), "").unwrap();
    (dir, runs)
}

fn default_log(dir: &TempDir) -> PathBuf {
    crate::backend::scm::default_log::path_in(&crate::backend::scm::folder::logs_folder(dir.path()), "nightly")
}

fn records_dir(dir: &TempDir) -> PathBuf {
    dir.path().join("Goetia").join("runs").join("nightly")
}

/// What the machine holds for a task that runs as `principal`, with the password given.
fn prepared(principal: Sid, password: Option<&str>) -> impl FnOnce(&TaskAccount) -> Result<Prepared> {
    let password = password.map(str::to_owned);
    move |_| Ok(Prepared { principal, password })
}

/// The call a registration under `token` makes.
fn register_call(
    spec: &DaemonSpec,
    armed: bool,
    who: TaskAccount,
    password: Option<&str>,
    update: bool,
    token: &Token,
) -> Call {
    Call::Register {
        path: PATH.to_owned(),
        xml: definition(spec).to_xml(armed, token),
        who,
        password: password.map(str::to_owned),
        update,
    }
}

fn root() -> TaskAccount {
    TaskAccount::Service("S-1-5-18".to_owned())
}

/// The token of the one registration the fake saw: the argument the action's arguments end with.
fn registered_token(fake: &Fake) -> Token {
    let tokens: Vec<_> = fake
        .calls()
        .iter()
        .filter_map(|call| match call {
            Call::Register { xml, .. } => {
                Some(TaskXml::token_of(xml, ID).unwrap().expect("a registration has a token"))
            }
            _ => None,
        })
        .collect();
    let [token] = tokens.as_slice() else {
        panic!("one registration expected: {tokens:?}")
    };
    *token
}

// Install =============================================================================================================

#[skuld::test]
fn install_of_a_task_creates_it_disarmed() {
    let (_dir, runs) = made_records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));

    let outcome = scm(&fake)
        .task_write_with(
            &spec,
            &definition(&spec),
            None,
            &runs,
            &mut Running(vec![]),
            prepared(system(), None),
        )
        .unwrap();

    assert!(matches!(outcome, WriteOutcome::Done), "{outcome:?}");
    assert_eq!(
        fake.calls(),
        [register_call(
            &spec,
            false,
            root(),
            None,
            false,
            &registered_token(&fake)
        )]
    );
    fake.finish();
}

#[skuld::test]
fn reinstall_keeps_the_armed_state() {
    let spec = task_spec(Some("30 3 * * *"));
    for armed in [true, false] {
        let (_dir, runs) = made_records();
        let fake = Fake::new();
        fake.on_register(Ok(Registration::Created));
        let installed = snapshot_of(&spec, armed);

        let outcome = scm(&fake)
            .task_write_with(
                &spec,
                &definition(&spec),
                Some(&installed),
                &runs,
                &mut Running(vec![]),
                prepared(system(), None),
            )
            .unwrap();

        assert!(matches!(outcome, WriteOutcome::Done), "{outcome:?}");
        assert_eq!(
            fake.calls(),
            [register_call(&spec, armed, root(), None, true, &task_token())],
            "an update of an armed={armed} task"
        );
        fake.finish();
    }
}

/// What `decide` compares is the definition's text, which has no armed state: an install of what is
/// installed is up to date whether or not the task is armed, so `write` is never reached.
#[skuld::test]
fn reinstall_of_an_identical_task_writes_nothing() {
    let spec = task_spec(Some("30 3 * * *"));
    let scm = Scm::new();
    let desired = definition(&spec).to_string();
    for armed in [true, false] {
        // Another registration's token is no difference.
        let other = Token::parse("00112233445566778899aabbccddeeff").unwrap();
        let installed = TaskSnapshot::new(id(), Ok(blob(&spec)), registered_task_with(&spec, armed, &other));
        let on_disk = scm.task_installed(&installed).unwrap().definition.to_string();
        let regenerated = scm.task_regenerate(&spec, &installed).unwrap().to_string();

        let outcome = decide::decide(
            &Ownership::Ours {
                blob: blob(&spec),
                regenerated,
                on_disk,
            },
            &desired,
            &spec,
            crate::version(),
            false,
            &ExternalConfig::Absent,
        );

        assert!(
            matches!(outcome, decide::Outcome::UpToDate),
            "armed={armed}: {outcome:?}"
        );
    }
}

#[skuld::test]
fn a_changed_task_is_updated_in_place_over_the_installed_one() {
    let spec = task_spec(Some("30 3 * * *"));
    let (_dir, runs) = made_records();
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));
    let installed = snapshot_of(&spec, false);
    let rescheduled = task_spec(Some("0 4 * * *"));

    scm(&fake)
        .task_write_with(
            &rescheduled,
            &definition(&rescheduled),
            Some(&installed),
            &runs,
            &mut Running(vec![]),
            prepared(system(), None),
        )
        .unwrap();

    assert_eq!(
        fake.calls(),
        [register_call(&rescheduled, false, root(), None, true, &task_token())]
    );
}

// The registration's token --------------------------------------------------------------------------------------------

/// A create of `spec` over nothing, and the token it registers.
fn token_of_a_create(spec: &DaemonSpec) -> Token {
    let (_dir, runs) = made_records();
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));
    scm(&fake)
        .task_write_with(
            spec,
            &definition(spec),
            None,
            &runs,
            &mut Running(vec![]),
            prepared(system(), None),
        )
        .unwrap();
    registered_token(&fake)
}

#[skuld::test]
fn create_makes_a_new_token() {
    let spec = task_spec(Some("30 3 * * *"));
    assert_ne!(token_of_a_create(&spec), token_of_a_create(&spec));
}

/// An update is the same registration: a shim that read the token before it is still the task's own.
#[skuld::test]
fn update_in_place_keeps_the_token() {
    let (_dir, runs) = made_records();
    let installed = snapshot_of(&task_spec(Some("30 3 * * *")), true);
    let rescheduled = task_spec(Some("0 4 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));

    scm(&fake)
        .task_write_with(
            &rescheduled,
            &definition(&rescheduled),
            Some(&installed),
            &runs,
            &mut Running(vec![]),
            prepared(system(), None),
        )
        .unwrap();

    assert_eq!(registered_token(&fake), task_token());
}

#[skuld::test]
fn an_account_change_makes_a_new_token() {
    let (_dir, runs) = made_records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true))
        .on_register(Ok(Registration::Created));

    scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, false)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap();

    assert_ne!(
        registered_token(&fake),
        task_token(),
        "the old registration's is not carried over"
    );
}

#[skuld::test]
fn retype_service_to_task_makes_a_new_token() {
    let spec = task_spec(Some("30 3 * * *"));
    let mut tokens = Vec::new();
    for _ in 0..2 {
        let (_dir, runs) = made_records();
        let fake = Fake::new();
        fake.on_register(Ok(Registration::Created));
        scm(&fake)
            .retype_to_task_with(
                &spec,
                &definition(&spec),
                &runs,
                prepared(process_user_sid(), None),
                || Ok(None),
                || Ok(()),
            )
            .unwrap();
        tokens.push(registered_token(&fake));
    }
    assert_ne!(tokens[0], tokens[1]);
}

/// A malformed token is a hand edit like any other: the task differs from what goetia wrote, and the update
/// writes it again under a well-formed token, armed as it was.
#[skuld::test]
fn an_update_over_a_malformed_token_writes_a_well_formed_one() {
    let spec = task_spec(Some("30 3 * * *"));
    for armed in [true, false] {
        let (_dir, runs) = made_records();
        let stored = registered_task(&spec, armed);
        let malformed = Registered {
            xml: stored
                .xml
                .replace(&format!(" {TASK_TOKEN}</Arguments>"), " 0123</Arguments>"),
            ..stored
        };
        assert_eq!(
            TaskXml::token_of(&malformed.xml, ID).unwrap(),
            None,
            "the edit made the token malformed"
        );
        let fake = Fake::new();
        fake.on_register(Ok(Registration::Created));

        scm(&fake)
            .task_write_with(
                &spec,
                &definition(&spec),
                Some(&TaskSnapshot::new(id(), Ok(blob(&spec)), malformed)),
                &runs,
                &mut Running(vec![]),
                prepared(system(), None),
            )
            .unwrap();

        // `registered_token` reads a token from the one registration the fake saw, so a well-formed one is there.
        assert_ne!(registered_token(&fake), task_token(), "armed={armed}: a fresh token");
        assert_eq!(
            fake.calls(),
            [register_call(
                &spec,
                armed,
                root(),
                None,
                true,
                &registered_token(&fake)
            )],
            "armed={armed}"
        );
    }
}

/// A stored registration whose XML cannot be read at all is no licence to write another: the write fails
/// before Task Scheduler is asked anything (no disable, delete or register), and no record directory is made.
/// An unreadable XML does not read as a changed account either.
#[skuld::test]
fn a_write_over_a_registration_whose_xml_cannot_be_read_fails_before_any_request() {
    let spec = task_spec(Some("30 3 * * *"));
    let unreadable = Registered {
        xml: "<Task><Actions>".to_owned(),
        ..registered_task(&spec, true)
    };
    let (dir, runs) = records();
    let fake = Fake::new();

    let failed = scm(&fake)
        .task_write_with(
            &spec,
            &definition(&spec),
            Some(&TaskSnapshot::new(id(), Ok(blob(&spec)), unreadable)),
            &runs,
            &mut Running(vec![]),
            prepared(system(), None),
        )
        .unwrap_err();

    assert!(matches!(failed, Error::Blob { .. }), "{failed:?}");
    assert_eq!(fake.calls(), []);
    assert!(!records_dir(&dir).exists());
}

// A change of account -------------------------------------------------------------------------------------------------

/// `Users`, which has this test's account among its members: a record directory that names only it is still
/// the test's to read and remove. (`NetworkService` would lock the test out of it.)
fn users() -> Sid {
    sid_from_parts(5, &[32, 545])
}

fn network_service() -> Sid {
    sid_from_parts(5, &[20])
}

/// The task as `name` (`LocalService` or `NetworkService`) runs it, and what a task of it registered as
/// that account's SID looks like to `read`.
fn as_service(name: &str) -> DaemonSpec {
    task_spec_as(User::Name(name.to_owned()), Some("30 3 * * *"))
}

fn service(sid: &str) -> TaskAccount {
    TaskAccount::Service(sid.to_owned())
}

/// A task installed over one of another account is registered again, not updated: Task Scheduler keeps the
/// security descriptor a task was registered with, so an update leaves the task unreadable by the new
/// account's shim. It is deleted and created, with the armed state it had, and its records stay.
#[skuld::test]
fn a_task_installed_as_another_account_is_deleted_and_created_with_the_armed_state_it_had() {
    for armed in [false, true] {
        let (dir, runs) = made_records();
        let old = as_service("LocalService");
        let new = as_service("NetworkService");
        let record = records_dir(&dir).join("0123456789abcdef0123456789abcdef.json");
        std::fs::write(&record, "{}").unwrap();
        let fake = Fake::new();
        fake.on_set_enabled(Ok(()))
            .on_instances(Ok(Vec::new()))
            .on_delete(Ok(true))
            .on_register(Ok(Registration::Created));

        let outcome = scm(&fake)
            .task_write_with(
                &new,
                &definition(&new),
                Some(&snapshot_of(&old, armed)),
                &runs,
                &mut Running(vec![]),
                prepared(users(), None),
            )
            .unwrap();

        assert!(matches!(outcome, WriteOutcome::Done), "{outcome:?}");
        // The disable is first: a scheduled fire between the check of the old account's runs and the delete
        // would start a run as the old account that nothing waits for.
        let token = registered_token(&fake);
        assert_ne!(
            token,
            task_token(),
            "a create: the old registration's token is not kept"
        );
        assert_eq!(
            fake.calls(),
            [
                Call::SetEnabled(PATH.to_owned(), false),
                Call::Instances(PATH.to_owned()),
                Call::Delete(PATH.to_owned()),
                register_call(&new, armed, service("S-1-5-20"), None, false, &token),
            ],
            "armed={armed}: disabled, checked, deleted, created"
        );
        assert!(record.is_file(), "the old account's record stays");
        assert!(
            trustees_of(&records_dir(&dir)).contains(&users()),
            "the directory names the new account"
        );
        fake.finish();
    }
}

/// The old account's run is refused before anything is changed: its shim still writes its record, so the
/// directory's permissions are not rewritten, and the task is not deleted.
#[skuld::test]
fn a_task_that_runs_is_not_installed_as_another_account() {
    let (dir, runs) = records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![running_run()]))
        .on_set_enabled(Ok(()));

    let refused = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![4242]),
            prepared(network_service(), None),
        )
        .unwrap_err();

    assert!(matches!(refused, Error::AccountChangeRunning { .. }), "{refused:?}");
    assert!(
        refused.to_string().contains("another account"),
        "the error names the account change: {refused}"
    );
    assert_eq!(
        fake.calls(),
        [
            Call::SetEnabled(PATH.to_owned(), false),
            Call::Instances(PATH.to_owned()),
            Call::SetEnabled(PATH.to_owned(), true),
        ],
        "the task is as it was: enabled again"
    );
    assert!(
        !records_dir(&dir).exists(),
        "the record directory's permissions are not rewritten while the old run writes in it"
    );
}

/// What the new account needs (its password, its logon right) is acquired before the task is touched:
/// a failure leaves it as it was, with the scheduler asked nothing.
#[skuld::test]
fn an_account_change_whose_acquisition_fails_touches_nothing() {
    let (dir, runs) = records();
    let old = as_service("LocalService");
    let new = task_spec_as(User::Name("alice".into()), Some("30 3 * * *"));
    let fake = Fake::new();

    let refused = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![]),
            |account| {
                password_of(account, || Ok(None)).map(|password| Prepared {
                    principal: system(),
                    password,
                })
            },
        )
        .unwrap_err();

    assert!(matches!(refused, Error::PasswordRequired { .. }), "{refused:?}");
    assert_eq!(fake.calls(), []);
    assert!(!records_dir(&dir).exists());
}

/// A task whose stored principal cannot be told is not known to be the same account, so it is registered
/// again.
#[skuld::test]
fn a_stored_principal_that_cannot_be_read_is_an_account_change() {
    let (_dir, runs) = made_records();
    let new = as_service("NetworkService");
    let installed = registered_task(&new, false);
    let no_principal = Registered {
        xml: installed.xml.replace("<UserId>S-1-5-20</UserId>", ""),
        data: installed.data.clone(),
        enabled: Some(true),
    };
    assert_ne!(no_principal.xml, installed.xml, "the principal is gone");
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true))
        .on_register(Ok(Registration::Created));

    scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&TaskSnapshot::new(id(), Ok(blob(&new)), no_principal)),
            &runs,
            &mut Running(vec![]),
            // An account the test is in: the default log is set twice, and the second open needs to read the
            // attributes the first set left only to the accounts it names.
            prepared(users(), None),
        )
        .unwrap();

    assert!(matches!(
        fake.calls().last(),
        Some(Call::Register { update: false, .. })
    ));
    assert_ne!(registered_token(&fake), task_token(), "a new registration: a new token");
    fake.finish();
}

/// A task that is the same account is updated in place and nothing is asked of its runs.
#[skuld::test]
fn a_task_installed_as_the_same_account_is_updated_in_place() {
    let (_dir, runs) = made_records();
    let old = as_service("NetworkService");
    let new = task_spec_as(User::Name("NetworkService".into()), Some("0 4 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));

    scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, false)),
            &runs,
            &mut Running(vec![]),
            prepared(network_service(), None),
        )
        .unwrap();

    assert_eq!(
        fake.calls(),
        [register_call(
            &new,
            false,
            service("S-1-5-20"),
            None,
            true,
            &task_token()
        )]
    );
}

/// A delete that took the task and could not remove the folder left empty is no failure: the task is gone,
/// and the create uses the folder that stayed.
#[skuld::test]
fn a_folder_that_stays_after_the_delete_does_not_stop_the_create() {
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let (_dir, runs) = made_records();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Err(Error::TaskFolderKept {
            task: PATH.to_owned(),
            folder: r"\Goetia".to_owned(),
            source: Box::new(Error::os("remove", std::io::Error::other("in use"))),
        }))
        .on_register(Ok(Registration::Created));

    let outcome = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, false)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap();

    assert!(matches!(outcome, WriteOutcome::Done), "{outcome:?}");
    fake.finish();
}

/// A create that finds a task there once the old one is deleted is raced, as any create is; a delete
/// that fails creates nothing.
#[skuld::test]
fn a_raced_create_and_a_failed_delete_after_an_account_change() {
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let (_dir, runs) = made_records();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true))
        .on_register(Ok(Registration::Raced))
        .on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Err(Error::os("delete", std::io::Error::other("denied"))))
        .on_set_enabled(Ok(()));
    let scm = scm(&fake);
    let write = || {
        scm.task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, false)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
    };

    let raced = write().unwrap();
    let failed = write().unwrap_err();

    assert!(matches!(raced, WriteOutcome::Raced), "{raced:?}");
    assert!(matches!(failed, Error::Os { .. }), "{failed:?}");
    assert_eq!(
        fake.calls()
            .iter()
            .filter(|call| matches!(call, Call::Register { .. }))
            .count(),
        1,
        "a delete that fails creates nothing"
    );
    fake.finish();
}

/// A delete that fails leaves the old task there, so the enabled flag is put back, and the delete's error is the
/// write's.
#[skuld::test]
fn a_failed_delete_after_the_disable_puts_the_enabled_flag_back() {
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let (_dir, runs) = made_records();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Err(Error::os("delete", std::io::Error::other("denied"))))
        .on_set_enabled(Ok(()));
    let failed = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap_err();
    assert!(failed.to_string().contains("delete"), "{failed}");
    assert_eq!(
        fake.calls(),
        [
            Call::SetEnabled(PATH.to_owned(), false),
            Call::Instances(PATH.to_owned()),
            Call::Delete(PATH.to_owned()),
            Call::SetEnabled(PATH.to_owned(), true),
        ]
    );
    fake.finish();
}

/// A failure after the delete is not a refusal that left the task: the old task is gone, nothing is installed at
/// the id, and an install that runs again creates it disarmed. No flag is put back on a task that is not there.
/// The error says to install again, and for a task that was armed says to arm it after; the failure itself stays
/// under it. The create that fails and the record directory that cannot be made are the two failures there.
#[skuld::test]
fn a_failure_after_the_delete_says_what_it_left() {
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    for armed in [true, false] {
        let (_dir, runs) = made_records();
        let fake = Fake::new();
        fake.on_set_enabled(Ok(()))
            .on_instances(Ok(Vec::new()))
            .on_delete(Ok(true))
            .on_register(Err(Error::os("register", std::io::Error::other("denied"))));

        let failed = scm(&fake)
            .task_write_with(
                &new,
                &definition(&new),
                Some(&snapshot_of(&old, armed)),
                &runs,
                &mut Running(vec![]),
                prepared(users(), None),
            )
            .unwrap_err();

        let Error::Step { what, source } = &failed else {
            panic!("armed={armed}: a failure after the delete is told as what it left: {failed:?}")
        };
        assert!(what.contains("old task") && what.contains("already removed"), "{what}");
        assert!(what.contains("nothing is installed"), "{what}");
        assert!(what.contains("re-run install"), "{what}");
        assert_eq!(
            what.contains("goetia daemon enable nightly"),
            armed,
            "armed={armed}: only a task that was armed is told to be enabled: {what}"
        );
        assert!(matches!(**source, Error::Os { .. }), "{source:?}");
        assert!(failed.report().to_string().contains("denied"), "{}", failed.report());
        assert_eq!(fake.calls().first(), Some(&Call::SetEnabled(PATH.to_owned(), false)));
        assert!(
            matches!(fake.calls().last(), Some(Call::Register { .. })),
            "no flag is put back on a task that is not there: {:?}",
            fake.calls()
        );
        fake.finish();
    }

    // The record directory cannot be made (its parents are not there): the same, and no create follows.
    let (_dir, runs) = records();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true));
    let failed = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap_err();
    let Error::Step { what, .. } = &failed else {
        panic!("a failure after the delete is told as what it left: {failed:?}")
    };
    assert!(
        what.contains("already removed") && what.contains("re-run install"),
        "{what}"
    );
    assert!(
        failed.report().to_string().contains("run records"),
        "{}",
        failed.report()
    );
    assert_eq!(
        fake.calls(),
        [
            Call::SetEnabled(PATH.to_owned(), false),
            Call::Instances(PATH.to_owned()),
            Call::Delete(PATH.to_owned()),
        ]
    );
    fake.finish();
}

/// The error after the delete names `enable` only where it can be run: the old task was armed and the new
/// definition has a schedule to arm. A definition with none leaves a task that `enable` refuses.
#[skuld::test]
fn a_failure_after_the_delete_names_enable_only_where_there_is_a_schedule_to_arm() {
    let old = as_service("LocalService");
    let schedules = [
        (as_service("NetworkService"), true),
        (task_spec_as(User::Name("NetworkService".to_owned()), None), false),
    ];
    for (new, names_enable) in schedules {
        let (_dir, runs) = made_records();
        let fake = Fake::new();
        fake.on_set_enabled(Ok(()))
            .on_instances(Ok(Vec::new()))
            .on_delete(Ok(true))
            .on_register(Err(Error::os("register", std::io::Error::other("denied"))));

        let failed = scm(&fake)
            .task_write_with(
                &new,
                &definition(&new),
                Some(&snapshot_of(&old, true)),
                &runs,
                &mut Running(vec![]),
                prepared(users(), None),
            )
            .unwrap_err();

        let Error::Step { what, .. } = &failed else {
            panic!("a failure after the delete is told as what it left: {failed:?}")
        };
        assert_eq!(
            what.contains("goetia daemon enable"),
            names_enable,
            "a schedule to arm: {names_enable}: {what}"
        );
        fake.finish();
    }
}

/// A marker that cannot be placed leaves nothing to take away: the flag is put back, and the one cause is the
/// one error (not a second one about a marker that was never placed). What stands at the marker's name is left.
#[skuld::test]
fn a_marker_that_cannot_be_placed_puts_back_the_flag_and_no_marker() {
    let (dir, runs) = made_records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let marker = dir.path().join("Goetia").join("runs").join("nightly~removing");
    std::fs::create_dir(&marker).unwrap();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(())).on_set_enabled(Ok(()));

    let failed = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap_err();

    let report = failed.report().to_string();
    assert!(report.contains("place the removal's marker"), "{report}");
    assert!(
        !report.contains("take the removal's marker away"),
        "one cause is one error: {report}"
    );
    assert_eq!(
        fake.calls(),
        [
            Call::SetEnabled(PATH.to_owned(), false),
            Call::SetEnabled(PATH.to_owned(), true)
        ]
    );
    assert!(marker.is_dir(), "nothing was taken away");
    fake.finish();
}

/// The marker is placed before the pins are read: with the marker refused the pin check is never reached, so the
/// stale pin it sweeps is still there.
#[skuld::test]
fn the_marker_is_placed_before_the_pins_are_read() {
    let (dir, runs) = made_records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    std::fs::create_dir(dir.path().join("Goetia").join("runs").join("nightly~removing")).unwrap();
    let stale = records_dir(&dir).join("pin-stale.pin");
    std::fs::write(&stale, "").unwrap();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(())).on_set_enabled(Ok(()));

    scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap_err();

    assert!(stale.exists(), "the pin check did not run");
    fake.finish();
}

/// A marker that cannot be taken away (something holds it open) is reported beside the failure that made it
/// necessary, a refusal and a failed delete alike; the flag is put back all the same.
#[skuld::test]
fn a_marker_that_cannot_be_taken_away_is_reported_with_the_failure() {
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    for ending in ["refused", "delete fails"] {
        let (dir, runs) = made_records();
        let holder = Arc::new(Mutex::new(None));
        let hold = {
            let (holder, marker) = (
                Arc::clone(&holder),
                dir.path().join("Goetia").join("runs").join("nightly~removing"),
            );
            move || {
                let file = OpenOptions::new()
                    .read(true)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                    .open(marker)
                    .unwrap();
                *holder.lock().unwrap() = Some(file);
            }
        };
        let fake = Fake::new();
        fake.on_set_enabled(Ok(()));
        let mut processes = Running(if ending == "refused" { vec![4242] } else { vec![] });
        if ending == "refused" {
            fake.on_instances(Ok(vec![running_run()])).during(hold);
        } else {
            fake.on_instances(Ok(Vec::new()))
                .during(hold)
                .on_delete(Err(Error::os("delete", std::io::Error::other("denied"))));
        }
        fake.on_set_enabled(Ok(()));

        let failed = scm(&fake)
            .task_write_with(
                &new,
                &definition(&new),
                Some(&snapshot_of(&old, true)),
                &runs,
                &mut processes,
                prepared(users(), None),
            )
            .unwrap_err();

        let report = failed.report().to_string();
        assert!(report.contains("take the removal's marker away"), "{ending}: {report}");
        let cause = if ending == "refused" {
            "goetia daemon stop"
        } else {
            "denied"
        };
        assert!(report.contains(cause), "{ending}: {report}");
        assert_eq!(
            fake.calls().last(),
            Some(&Call::SetEnabled(PATH.to_owned(), true)),
            "{ending}: the flag is put back"
        );
        fake.finish();
    }
}

/// With `runs` gone there are no records, so no marker is placed and none is taken away: a refusal is the one
/// error, and the flag is put back.
#[skuld::test]
fn an_account_change_with_no_records_places_no_marker_and_takes_none_away() {
    let (dir, runs) = made_records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    std::fs::remove_dir_all(dir.path().join("Goetia").join("runs")).unwrap();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![running_run()]))
        .on_set_enabled(Ok(()));

    let refused = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![4242]),
            prepared(users(), None),
        )
        .unwrap_err();

    assert!(matches!(refused, Error::AccountChangeRunning { .. }), "{refused:?}");
    assert!(!runs.is_closed().unwrap(), "no marker was placed");
    fake.finish();
}

/// A flag that cannot be put back is reported with the failure that made it necessary.
#[skuld::test]
fn an_enabled_flag_that_cannot_be_put_back_is_reported_with_the_failure() {
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let (_dir, runs) = made_records();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Err(Error::os("delete", std::io::Error::other("denied"))))
        .on_set_enabled(Err(Error::os("enable", std::io::Error::other("busy"))));

    let failed = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap_err();

    let report = failed.report().to_string();
    assert!(report.contains("denied") && report.contains("busy"), "{report}");
    fake.finish();
}

/// The task's own flag is true after every outcome, whatever Task Scheduler said of it when it was read (off,
/// not said, on): the change disables it whatever it found, and a refusal and a failure each put it back true.
/// A change that lands writes a registration whose flag is true.
#[skuld::test]
fn the_own_flag_is_true_after_every_outcome() {
    let disabled = Call::SetEnabled(PATH.to_owned(), false);
    let enabled_again = Call::SetEnabled(PATH.to_owned(), true);
    let instances = Call::Instances(PATH.to_owned());
    let delete = Call::Delete(PATH.to_owned());
    for found_flag in [Some(false), None, Some(true)] {
        let old = as_service("LocalService");
        let new = as_service("NetworkService");
        let found = || Registered {
            enabled: found_flag,
            ..registered_task(&old, true)
        };
        let change = |fake: &Fake, runs: &Runs, processes: &mut Running| {
            scm(fake).task_write_with(
                &new,
                &definition(&new),
                Some(&TaskSnapshot::new(id(), Ok(blob(&old)), found())),
                runs,
                processes,
                prepared(users(), None),
            )
        };

        // Refused: a run is alive.
        let (_dir, runs) = made_records();
        let fake = Fake::new();
        fake.on_set_enabled(Ok(()))
            .on_instances(Ok(vec![running_run()]))
            .on_set_enabled(Ok(()));
        let refused = change(&fake, &runs, &mut Running(vec![4242])).unwrap_err();
        assert!(matches!(refused, Error::AccountChangeRunning { .. }), "{refused:?}");
        assert_eq!(
            fake.calls(),
            [disabled.clone(), instances.clone(), enabled_again.clone()],
            "{found_flag:?}: refused"
        );
        fake.finish();

        // The delete fails: the task is there, enabled again.
        let (_dir, runs) = made_records();
        let fake = Fake::new();
        fake.on_set_enabled(Ok(()))
            .on_instances(Ok(Vec::new()))
            .on_delete(Err(Error::os("delete", std::io::Error::other("denied"))))
            .on_set_enabled(Ok(()));
        let failed = change(&fake, &runs, &mut Running(vec![])).unwrap_err();
        assert!(failed.to_string().contains("delete"), "{found_flag:?}: {failed}");
        assert_eq!(
            fake.calls(),
            [
                disabled.clone(),
                instances.clone(),
                delete.clone(),
                enabled_again.clone()
            ],
            "{found_flag:?}: delete failed"
        );
        fake.finish();

        // The create fails: no task is there, and no flag is put back on one.
        let (_dir, runs) = made_records();
        let fake = Fake::new();
        fake.on_set_enabled(Ok(()))
            .on_instances(Ok(Vec::new()))
            .on_delete(Ok(true))
            .on_register(Err(Error::os("register", std::io::Error::other("denied"))));
        let failed = change(&fake, &runs, &mut Running(vec![])).unwrap_err();
        assert!(
            failed.report().to_string().contains("register"),
            "{found_flag:?}: {}",
            failed.report()
        );
        assert!(
            matches!(fake.calls().last(), Some(Call::Register { .. })),
            "{found_flag:?}: create failed, and nothing is enabled after it"
        );
        fake.finish();

        // Done: the registration written has the flag true (a trigger-armed task is written enabled).
        let (_dir, runs) = made_records();
        let fake = Fake::new();
        fake.on_set_enabled(Ok(()))
            .on_instances(Ok(Vec::new()))
            .on_delete(Ok(true))
            .on_register(Ok(Registration::Created));
        change(&fake, &runs, &mut Running(vec![])).unwrap();
        let written = fake
            .calls()
            .into_iter()
            .find_map(|call| match call {
                Call::Register { xml, .. } => Some(xml),
                _ => None,
            })
            .expect("a registration");
        assert!(TaskXml::armed(&written), "{found_flag:?}: done: {written}");
        fake.finish();
    }
}

/// The armed state of the new registration is the old one's triggers and nothing else: a task whose own flag was
/// off (a change or an uninstall that was cut short disabled it) or not said is still created armed if it had
/// triggers, and one with none is created without. The removal's marker stands in each case, as an uninstall that
/// died leaves it with the flag off, and the change takes it away.
#[skuld::test]
fn account_change_takes_armed_from_the_triggers_alone() {
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let with_flag_off = |armed: bool| registered_flag_off(&old, armed);
    let unread = |armed: bool| Registered {
        enabled: None,
        ..registered_task(&old, armed)
    };
    for (found, armed) in [
        (with_flag_off(true), true),
        (with_flag_off(false), false),
        (unread(true), true),
        (unread(false), false),
    ] {
        let (_dir, runs) = made_records();
        runs.close().unwrap();
        let fake = Fake::new();
        fake.on_set_enabled(Ok(()))
            .on_instances(Ok(Vec::new()))
            .on_delete(Ok(true))
            .on_register(Ok(Registration::Created));

        scm(&fake)
            .task_write_with(
                &new,
                &definition(&new),
                Some(&TaskSnapshot::new(id(), Ok(blob(&old)), found)),
                &runs,
                &mut Running(vec![]),
                prepared(users(), None),
            )
            .unwrap();

        let token = registered_token(&fake);
        assert_eq!(
            fake.calls().last(),
            Some(&register_call(&new, armed, service("S-1-5-20"), None, false, &token)),
            "armed={armed}"
        );
        assert!(!runs.is_closed().unwrap(), "armed={armed}: the marker is taken away");
        fake.finish();
    }
}

/// What Task Scheduler holds of `spec`'s task with its own flag off and, as `triggers` says, with or without
/// triggers: what an account change or an uninstall that was cut short leaves.
fn registered_flag_off(spec: &DaemonSpec, triggers: bool) -> Registered {
    let registered = registered_task(spec, triggers);
    Registered {
        xml: TaskXml::with_own_flag(&registered.xml, false).unwrap(),
        enabled: Some(false),
        ..registered
    }
}

/// A forced install, or one that finds the blob stale (a plain one is a conflict, the flag being compared), over a
/// task of the same account whose own flag is off (a change that was cut short disabled it) is armed as its
/// triggers say, as an account change is, and writes the flag true: a task with triggers is not silently
/// disarmed, and one with none stays so. The removal's marker an uninstall left (the flag is off with it) is
/// taken away by the write.
#[skuld::test]
fn an_update_of_a_task_with_its_flag_off_is_armed_by_its_triggers_alone() {
    let spec = task_spec(Some("30 3 * * *"));
    let rescheduled = task_spec(Some("0 4 * * *"));
    for triggers in [true, false] {
        let (_dir, runs) = made_records();
        runs.close().unwrap();
        let fake = Fake::new();
        fake.on_register(Ok(Registration::Created));
        let installed = TaskSnapshot::new(id(), Ok(blob(&spec)), registered_flag_off(&spec, triggers));

        scm(&fake)
            .task_write_with(
                &rescheduled,
                &definition(&rescheduled),
                Some(&installed),
                &runs,
                &mut Running(vec![]),
                prepared(system(), None),
            )
            .unwrap();

        assert_eq!(
            fake.calls(),
            [register_call(&rescheduled, triggers, root(), None, true, &task_token())],
            "triggers={triggers}"
        );
        assert!(
            !runs.is_closed().unwrap(),
            "triggers={triggers}: the marker is taken away"
        );
        fake.finish();
    }
}

// The order of an account change --------------------------------------------------------------------------------------

/// What the world looks like to one call of the fake, noted as the call is made: `at`, whether the removal's
/// marker stands, and whether the record directory names the new account ([`users`]).
struct Seen(Arc<Mutex<Vec<String>>>);

impl Seen {
    fn new() -> Self {
        Seen(Arc::new(Mutex::new(Vec::new())))
    }

    /// The effect that notes the world at `at`, for the task under `dir`.
    fn at(&self, at: &'static str, dir: &TempDir) -> impl FnOnce() + Send + 'static {
        let (seen, runs, records) = (Arc::clone(&self.0), Runs::of(dir.path(), &id()), records_dir(dir));
        move || {
            let marker = runs.is_closed().unwrap();
            let narrowed = trustees_of(&records).contains(&users());
            seen.lock()
                .unwrap()
                .push(format!("{at}: marker={marker} narrowed={narrowed}"));
        }
    }

    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// An account change goes in order: disable, marker, the check of the pins, the listing, the delete, the records
/// narrowed to the new account (which takes the marker away), the create.
#[skuld::test]
fn account_change_order() {
    let (dir, runs) = made_records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let seen = Seen::new();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .during(seen.at("disable", &dir))
        .on_instances(Ok(Vec::new()))
        .during(seen.at("listing", &dir))
        .on_delete(Ok(true))
        .during(seen.at("delete", &dir))
        .on_register(Ok(Registration::Created))
        .during(seen.at("create", &dir));

    scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap();

    assert_eq!(
        seen.all(),
        [
            "disable: marker=false narrowed=false",
            "listing: marker=true narrowed=false",
            "delete: marker=true narrowed=false",
            "create: marker=false narrowed=true",
        ]
    );
    fake.finish();
}

/// A pin held by something that is no run of the task (a shim that is starting, a `run`, a stop, another
/// program) refuses the change before the delete: the listing names no instance, so the refusal says the records
/// are held. The task is enabled again, the marker is gone, and the records' permissions are not touched.
#[skuld::test]
fn held_pin_with_no_run_listed_refuses_the_change_before_any_delete() {
    let (dir, runs) = made_records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let before = trustees_of(&records_dir(&dir));
    let pin = runs.hold().unwrap();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_set_enabled(Ok(()));

    let refused = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap_err();

    let Error::AccountChangeHeld { pin: held, .. } = &refused else {
        panic!("a pin holds the records and no run is listed: {refused:?}")
    };
    assert_eq!(held.parent(), Some(records_dir(&dir).as_path()));
    assert!(refused.to_string().contains("run records"), "{refused}");
    assert_eq!(
        fake.calls(),
        [
            Call::SetEnabled(PATH.to_owned(), false),
            Call::Instances(PATH.to_owned()),
            Call::SetEnabled(PATH.to_owned(), true)
        ]
    );
    assert_eq!(
        trustees_of(&records_dir(&dir)),
        before,
        "the permissions are not touched"
    );
    assert!(!runs.is_closed().unwrap(), "the marker is taken away");
    drop(pin);
    fake.finish();
}

/// A running task holds its pin for the whole run, so an ordinary running task has a pin held and an instance
/// listed. The refusal then says a run is alive and names `goetia daemon stop`, not that the records are held.
#[skuld::test]
fn a_running_task_with_its_pin_held_is_refused_as_running() {
    let (dir, runs) = made_records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let pin = runs.hold().unwrap();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![running_run()]))
        .on_set_enabled(Ok(()));

    let refused = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![4242]),
            prepared(users(), None),
        )
        .unwrap_err();

    assert!(matches!(refused, Error::AccountChangeRunning { .. }), "{refused:?}");
    assert!(refused.to_string().contains("goetia daemon stop"), "{refused}");
    assert_eq!(
        fake.calls(),
        [
            Call::SetEnabled(PATH.to_owned(), false),
            Call::Instances(PATH.to_owned()),
            Call::SetEnabled(PATH.to_owned(), true)
        ]
    );
    assert!(!runs.is_closed().unwrap(), "the marker is taken away");
    assert!(records_dir(&dir).is_dir());
    drop(pin);
    fake.finish();
}

/// A pin whose state cannot be told may be held: the change fails before the listing and the delete, the marker
/// is taken away, the flag is put back, and nothing is sent to delete the task.
#[skuld::test]
fn unjudgeable_pin_fails_the_change_before_the_delete() {
    let (dir, runs) = made_records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let records = records_dir(&dir);
    std::fs::write(records.join("pin-odd.pin"), "").unwrap();
    let refused_delete = Undeletable::new(&records, &["pin-odd.pin"]);
    let fake = Fake::new();
    fake.on_set_enabled(Ok(())).on_set_enabled(Ok(()));

    let failed = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap_err();

    assert!(failed.report().to_string().contains("pin-odd.pin"), "{failed}");
    assert_eq!(
        fake.calls(),
        [
            Call::SetEnabled(PATH.to_owned(), false),
            Call::SetEnabled(PATH.to_owned(), true)
        ],
        "nothing is listed or deleted"
    );
    assert!(!runs.is_closed().unwrap(), "the marker is taken away");
    drop(refused_delete);
    fake.finish();
}

/// What the records' creation would refuse is refused before anything is sent to Task Scheduler, and before the
/// marker, which the refusal would otherwise have to take away: a link where the task's directory should be, and
/// at its default log's name a directory, a link and a delete that is pending. The old task is not disabled, so
/// no scheduled run in the window is skipped. Each refusal says what it is: no other case's words are in it.
#[skuld::test]
fn what_the_records_will_refuse_is_refused_before_the_delete() {
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let says = [
        "is a link, which goetia did not make; uninstalling the task removes it",
        "is a directory, which goetia did not make",
        "is a link, which goetia did not make; delete it and install the daemon again",
        "is being deleted while something still has it open",
    ];
    for (what, own) in [
        "a link at runs",
        "a directory at the log",
        "a link at the log",
        "a delete pending at the log",
    ]
    .into_iter()
    .zip(says)
    {
        let (dir, runs) = made_records();
        let target = tempfile::tempdir().unwrap();
        let mut _pending = None;
        match what {
            "a link at runs" => {
                std::fs::remove_dir(records_dir(&dir)).unwrap();
                junction(&records_dir(&dir), target.path());
            }
            "a directory at the log" => {
                std::fs::remove_file(default_log(&dir)).unwrap();
                std::fs::create_dir(default_log(&dir)).unwrap();
            }
            "a link at the log" => {
                std::fs::remove_file(default_log(&dir)).unwrap();
                std::fs::create_dir(default_log(&dir)).unwrap();
                make_mount_point(&default_log(&dir), target.path());
            }
            _ => {
                std::fs::remove_file(default_log(&dir)).unwrap();
                _pending = Some(delete_pending(&default_log(&dir)));
            }
        }
        let fake = Fake::new();

        let failed = scm(&fake)
            .task_write_with(
                &new,
                &definition(&new),
                Some(&snapshot_of(&old, true)),
                &runs,
                &mut Running(vec![]),
                prepared(users(), None),
            )
            .unwrap_err();

        assert!(matches!(failed, Error::Os { .. }), "{what}: {failed:?}");
        let report = failed.report().to_string();
        for phrase in says {
            assert_eq!(report.contains(phrase), phrase == own, "{what}: `{phrase}` in {report}");
        }
        assert_eq!(fake.calls(), [], "{what}: nothing is sent to Task Scheduler");
        assert!(!runs.is_closed().unwrap(), "{what}: no marker is placed");
        fake.finish();
    }
}

/// A run that is listed only after the marker was placed refuses the change: the marker precedes the listing, so
/// no shim can start unseen between them.
#[skuld::test]
fn account_change_refuses_a_run_listed_after_the_marker() {
    let (dir, runs) = made_records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let seen = Seen::new();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(vec![running_run()]))
        .during(seen.at("listing", &dir))
        .on_set_enabled(Ok(()));

    let refused = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![4242]),
            prepared(users(), None),
        )
        .unwrap_err();

    assert!(matches!(refused, Error::AccountChangeRunning { .. }), "{refused:?}");
    assert_eq!(seen.all(), ["listing: marker=true narrowed=false"]);
    assert!(!runs.is_closed().unwrap(), "the marker is taken away");
    fake.finish();
}

/// A delete that fails leaves the old task, so it takes the marker away as a refusal does: the task's shims
/// start again.
#[skuld::test]
fn a_failed_delete_takes_the_marker_away() {
    let (_dir, runs) = made_records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Err(Error::os("delete", std::io::Error::other("denied"))))
        .on_set_enabled(Ok(()));

    let failed = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap_err();

    assert!(failed.to_string().contains("delete"), "{failed}");
    assert!(!runs.is_closed().unwrap());
    fake.finish();
}

/// A change whose create failed leaves nothing at the id and no marker; the install that runs again creates the
/// task (disarmed, as any create is).
#[skuld::test]
fn rerun_after_a_failed_create_installs_and_clears_the_marker() {
    let (_dir, runs) = made_records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true))
        .on_register(Err(Error::os("register", std::io::Error::other("denied"))))
        .on_register(Ok(Registration::Created));
    let scm = scm(&fake);

    scm.task_write_with(
        &new,
        &definition(&new),
        Some(&snapshot_of(&old, true)),
        &runs,
        &mut Running(vec![]),
        prepared(users(), None),
    )
    .unwrap_err();
    assert!(!runs.is_closed().unwrap(), "the marker went with the narrowing");
    let again = scm
        .task_write_with(
            &new,
            &definition(&new),
            None,
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap();

    assert!(matches!(again, WriteOutcome::Done), "{again:?}");
    assert!(!runs.is_closed().unwrap());
    fake.finish();
}

/// A change that died leaves either the disabled task and the marker, or nothing at the id and the marker. The
/// install that runs again finishes either: the first disables again and places the marker again; the second
/// creates and clears it.
#[skuld::test]
fn rerun_after_a_dead_change_with_the_marker_left_proceeds() {
    let old = as_service("LocalService");
    let new = as_service("NetworkService");

    let (_dir, runs) = made_records();
    runs.close().unwrap();
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true))
        .on_register(Ok(Registration::Created));
    let outcome = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, true)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap();
    assert!(matches!(outcome, WriteOutcome::Done), "{outcome:?}");
    assert!(!runs.is_closed().unwrap());
    fake.finish();

    let (_dir, runs) = made_records();
    runs.close().unwrap();
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));
    let outcome = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            None,
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap();
    assert!(matches!(outcome, WriteOutcome::Done), "{outcome:?}");
    assert!(!runs.is_closed().unwrap());
    fake.finish();
}

// The record directory ------------------------------------------------------------------------------------------------

fn trustees_of(dir: &std::path::Path) -> Vec<Sid> {
    let file = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .unwrap();
    let security = read_object(&file, dir.to_path_buf()).unwrap();
    security
        .dacl
        .expect("the directory has a DACL")
        .into_iter()
        .map(|ace| ace.trustee)
        .collect()
}

/// A reinstall as another user must leave the shim, which runs as that user, able to write its
/// records and append to its default log: the permissions of the directory and of the log are rewritten by
/// every write, an update included.
#[skuld::test]
fn reinstall_with_another_user_rewrites_the_record_directorys_dacl() {
    let (dir, runs) = made_records();
    let spec = task_spec(Some("30 3 * * *"));
    let me = process_user_sid();
    let users = users();
    let fake = Fake::new();
    // The second write changes the account, so the task is deleted and created.
    fake.on_register(Ok(Registration::Created))
        .on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true))
        .on_register(Ok(Registration::Created));
    let scm = scm(&fake);
    scm.task_write_with(
        &spec,
        &definition(&spec),
        None,
        &runs,
        &mut Running(vec![]),
        prepared(me.clone(), None),
    )
    .unwrap();
    assert!(trustees_of(&records_dir(&dir)).contains(&me));
    assert!(trustees_of(&default_log(&dir)).contains(&me));

    scm.task_write_with(
        &spec,
        &definition(&spec),
        Some(&snapshot_of(&spec, false)),
        &runs,
        &mut Running(vec![]),
        prepared(users.clone(), None),
    )
    .unwrap();

    let trustees = trustees_of(&records_dir(&dir));
    assert!(trustees.contains(&users), "the new account is named");
    assert!(!trustees.contains(&me), "the old account is not");
    let trustees = trustees_of(&default_log(&dir));
    assert!(
        trustees.contains(&users),
        "the new account may append to the default log"
    );
    assert!(!trustees.contains(&me), "the old account may not");
}

/// The default log of `dir` with an entry for the test's own account, one that no install names now: what
/// is on the file is computed by the install, so the entry is not carried over.
fn log_with_an_entry_no_install_names(dir: &TempDir) {
    crate::backend::scm::default_log::ensure(
        dir.path(),
        &id(),
        &crate::backend::scm::default_log::Naming::alike(&[process_user_sid()]),
    )
    .unwrap();
}

fn local_service() -> Sid {
    sid_from_parts(5, &[19])
}

/// A change of account names the new account alone in the log and the records as soon as the old registration
/// is deleted, whether the create then lands, is raced or fails: no run of the old account can be left. A delete
/// that fails leaves the old task, and both as they were. What the log held is not read: an entry no install
/// names is gone.
#[skuld::test]
fn an_account_change_narrows_the_log_and_the_records_at_the_delete_and_not_before() {
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    for ending in ["raced", "register fails", "delete fails"] {
        let (dir, runs) = made_records();
        log_with_an_entry_no_install_names(&dir);
        let (log_before, records_before) = (trustees_of(&default_log(&dir)), trustees_of(&records_dir(&dir)));
        let fake = Fake::new();
        fake.on_set_enabled(Ok(())).on_instances(Ok(Vec::new()));
        match ending {
            "raced" => {
                fake.on_delete(Ok(true)).on_register(Ok(Registration::Raced));
            }
            "register fails" => {
                fake.on_delete(Ok(true))
                    .on_register(Err(Error::os("register", std::io::Error::other("denied"))));
            }
            _ => {
                fake.on_delete(Err(Error::os("delete", std::io::Error::other("denied"))))
                    .on_set_enabled(Ok(()));
            }
        }

        let _ = scm(&fake).task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, false)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        );

        let (log, records) = (trustees_of(&default_log(&dir)), trustees_of(&records_dir(&dir)));
        if ending == "delete fails" {
            assert_eq!(log, log_before, "{ending}: the log is as it was");
            assert_eq!(records, records_before, "{ending}: the records are as they were");
            continue;
        }
        for (what, trustees) in [("log", log), ("records", records)] {
            assert!(trustees.contains(&users()), "{ending}: the new account has its {what}");
            assert!(
                !trustees.contains(&local_service()) && !trustees.contains(&process_user_sid()),
                "{ending}: the {what} name no other account"
            );
        }
    }
}

/// A change that lands narrows the record directory to the new account alone.
#[skuld::test]
fn a_change_of_account_that_lands_leaves_the_new_account_alone_in_the_records_directory() {
    let (dir, runs) = made_records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true))
        .on_register(Ok(Registration::Created));

    scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, false)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap();

    let trustees = trustees_of(&records_dir(&dir));
    assert!(trustees.contains(&users()));
    assert!(!trustees.contains(&local_service()));
}

/// An account change from a snapshot whose stored principal cannot be told names the new account alone in the
/// record directory and the log.
#[skuld::test]
fn an_account_change_from_an_untold_principal_names_the_new_account_alone() {
    let (dir, runs) = made_records();
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let installed = registered_task(&old, false);
    let no_principal = Registered {
        xml: installed.xml.replace("<UserId>S-1-5-19</UserId>", ""),
        data: installed.data.clone(),
        enabled: Some(true),
    };
    assert_ne!(no_principal.xml, installed.xml, "the principal is gone");
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true))
        .on_register(Ok(Registration::Created));

    let wrote = scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&TaskSnapshot::new(id(), Ok(blob(&old)), no_principal)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap();

    assert!(matches!(wrote, WriteOutcome::Done), "{wrote:?}");
    for (what, trustees) in [
        ("the records", trustees_of(&records_dir(&dir))),
        ("the log", trustees_of(&default_log(&dir))),
    ] {
        assert!(trustees.contains(&users()), "{what} name the new account");
        assert!(!trustees.contains(&local_service()), "{what} do not name the old");
    }
    fake.finish();
}

/// A change that lands leaves the new account alone: the registered account's entry and anything else on the
/// file are gone.
#[skuld::test]
fn a_change_of_account_that_lands_leaves_the_new_account_alone_in_the_log() {
    let (dir, runs) = made_records();
    log_with_an_entry_no_install_names(&dir);
    let old = as_service("LocalService");
    let new = as_service("NetworkService");
    let fake = Fake::new();
    fake.on_set_enabled(Ok(()))
        .on_instances(Ok(Vec::new()))
        .on_delete(Ok(true))
        .on_register(Ok(Registration::Created));

    scm(&fake)
        .task_write_with(
            &new,
            &definition(&new),
            Some(&snapshot_of(&old, false)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap();

    let trustees = trustees_of(&default_log(&dir));
    assert!(trustees.contains(&users()));
    assert!(!trustees.contains(&local_service()));
    assert!(!trustees.contains(&process_user_sid()));
}

/// A task installed again as the account it has, which the stored principal says, names that account alone.
#[skuld::test]
fn an_unchanged_install_names_the_account_alone_in_the_log() {
    let (dir, runs) = made_records();
    log_with_an_entry_no_install_names(&dir);
    // An account the test is in, so that it may read what the install sets.
    let spec = task_spec_as(User::Id(AccountId::Sid("S-1-5-32-545".to_owned())), Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));

    scm(&fake)
        .task_write_with(
            &spec,
            &definition(&spec),
            Some(&snapshot_of(&spec, false)),
            &runs,
            &mut Running(vec![]),
            prepared(users(), None),
        )
        .unwrap();

    let trustees = trustees_of(&default_log(&dir));
    assert!(trustees.contains(&users()));
    assert!(!trustees.contains(&process_user_sid()));
}

// An install that finds the task up to date ---------------------------------------------------------------------------

/// [`task_beside_with`] under the temporary `%ProgramData%` `dir`, whose folders are judged as `Disable` (goetia's
/// own check could not pass there).
fn beside_of(
    dir: &TempDir,
    spec: &DaemonSpec,
    snapshot: &TaskSnapshot,
    prepare: impl FnOnce(&TaskAccount, Option<Written<'_>>, Password) -> Result<Prepared>,
) -> Result<()> {
    task_beside_with(
        dir.path(),
        &FolderCheck::new(CheckPermissions::Disable),
        spec,
        &definition(spec),
        snapshot,
        prepare,
    )
}

/// [`beside_of`] for [`users_spec`], with a `prepare` that looks at the account alone.
fn beside_with(
    dir: &TempDir,
    snapshot: &TaskSnapshot,
    prepare: impl FnOnce(&TaskAccount) -> Result<Prepared>,
) -> Result<()> {
    beside_of(dir, &users_spec(), snapshot, |account, _, _| prepare(account))
}

/// The task of `Users` (an account the test is in, so that it may read what the install sets).
fn users_spec() -> DaemonSpec {
    task_spec_as(User::Id(AccountId::Sid("S-1-5-32-545".to_owned())), Some("30 3 * * *"))
}

/// The account the definition of [`users_spec`] registers, which is the account its snapshot is registered as.
fn users_account() -> TaskAccount {
    definition(&users_spec()).account()
}

/// A task of `Users` as an install made it.
fn made_by_an_install() -> (TempDir, Runs, TaskSnapshot) {
    let (dir, runs) = records();
    let snapshot = snapshot_of(&users_spec(), false);
    beside_with(&dir, &snapshot, prepared(users(), None)).unwrap();
    assert!(records_dir(&dir).is_dir());
    (dir, runs, snapshot)
}

#[track_caller]
fn assert_beside_remakes(dir: &TempDir, runs: &Runs, snapshot: &TaskSnapshot) {
    beside_with(dir, snapshot, prepared(users(), None)).unwrap();
    assert!(runs.hold().is_ok(), "a run of the task does not back off");

    assert!(records_dir(dir).is_dir(), "the record directory is there");
    assert!(trustees_of(&records_dir(dir)).contains(&users()), "naming the account");
    assert!(default_log(dir).is_file(), "and the default log");
}

/// An install that finds the task up to date makes its record directory again, which the shim only opens.
/// Making the folders names Administrators as their owner, which only an elevated token may.
#[skuld::test]
fn an_up_to_date_install_makes_a_deleted_record_directory_again(#[fixture(scm_group)] _group: &Group) {
    let (dir, runs, snapshot) = made_by_an_install();
    std::fs::remove_dir(records_dir(&dir)).unwrap();

    assert_beside_remakes(&dir, &runs, &snapshot);
}

/// ... and `runs` itself, which every task's directory is in.
#[skuld::test]
fn an_up_to_date_install_makes_a_deleted_runs_folder_again(#[fixture(scm_group)] _group: &Group) {
    let (dir, runs, snapshot) = made_by_an_install();
    std::fs::remove_dir_all(dir.path().join("Goetia").join("runs")).unwrap();

    assert_beside_remakes(&dir, &runs, &snapshot);
}

/// ... and takes away the marker an uninstall that died left, which makes a `run` of the id back off.
#[skuld::test]
fn an_up_to_date_install_clears_a_marker_a_dead_uninstall_left(#[fixture(scm_group)] _group: &Group) {
    let (dir, runs, snapshot) = made_by_an_install();
    let marker = dir.path().join("Goetia").join("runs").join("nightly~removing");
    std::fs::write(&marker, "").unwrap();
    assert!(runs.hold().is_err(), "a run backs off while the marker is there");

    assert_beside_remakes(&dir, &runs, &snapshot);

    assert!(!marker.exists());
    assert!(runs.hold().is_ok());
}

/// An install cut short after the directory named both accounts leaves it so until the next install, which
/// finds the task up to date and names the registered account alone.
#[skuld::test]
fn an_up_to_date_install_names_the_registered_account_alone_in_the_record_directory(
    #[fixture(scm_group)] _group: &Group,
) {
    let (dir, runs, snapshot) = made_by_an_install();
    runs.ensure(&[users(), local_service()]).unwrap();
    assert!(trustees_of(&records_dir(&dir)).contains(&local_service()));

    assert_beside_remakes(&dir, &runs, &snapshot);

    assert!(!trustees_of(&records_dir(&dir)).contains(&local_service()));
}

/// ... and sees to what a write sees to for the spec's account (the event source, a real user's logon right),
/// written as the spec writes it, which a refusal quotes. It reads no password: the task is not registered
/// again. A failure of it fails the install.
#[skuld::test]
fn an_up_to_date_install_sees_to_what_the_machine_must_hold_for_the_specs_account(
    #[fixture(scm_group)] _group: &Group,
) {
    let (dir, _runs, snapshot) = made_by_an_install();
    let seen = std::cell::Cell::new(false);

    beside_of(&dir, &users_spec(), &snapshot, |account, written, reads| {
        assert_eq!(account, &users_account());
        assert!(matches!(written, Some(Written::Sid("S-1-5-32-545"))), "{written:?}");
        assert!(matches!(reads, Password::Unneeded), "no password is read");
        seen.set(true);
        Ok(Prepared {
            principal: users(),
            password: None,
        })
    })
    .unwrap();
    assert!(seen.get());

    let refused = beside_with(&dir, &snapshot, |_| {
        Err(Error::InvalidPassword { variable: "V".into() })
    });
    assert!(matches!(refused, Err(Error::InvalidPassword { .. })), "{refused:?}");
}

/// The error for a spec account that does not resolve quotes the spec's own field. The record directory is not
/// made, though the folders above it may be, and nothing the account needs is set: the lookup comes before the
/// logon right, the event source, the records and the log.
#[skuld::test]
fn an_up_to_date_install_of_an_unresolvable_spec_account_quotes_the_spec_field(#[fixture(scm_group)] _group: &Group) {
    let (dir, _runs) = records();
    let name = "goetia-no-such-account";
    let spec = task_spec_as(User::Name(name.to_owned()), Some("30 3 * * *"));
    let snapshot = snapshot_of(&spec, false);

    let failed = beside_of(&dir, &spec, &snapshot, prepare).unwrap_err();

    let text = failed.to_string();
    assert!(text.contains("user.name") && text.contains(name), "{text}");
    assert!(!records_dir(&dir).exists(), "`runs\\<id>` is absent");
    assert!(!default_log(&dir).exists(), "the log is not made");
}

/// The error of the record directory counts the accounts it names, an account named twice once.
#[skuld::test]
fn the_error_of_the_record_directory_counts_the_accounts_once_each() {
    assert_eq!(whose(&[users()]), "its account");
    assert_eq!(whose(&[users(), users()]), "its account");
    assert_eq!(whose(&[users(), local_service()]), "its accounts");
}

// Before any write ====================================================================================================

/// A real account's password is the one thing `password_of` reads, and only for a real account: a value
/// left over from installing another daemon must never reach a built-in account.
#[skuld::test]
fn a_real_account_needs_its_password_and_a_service_account_is_never_given_one() {
    let user = TaskAccount::User("alice".to_owned());

    let error = password_of(&user, || Ok(None)).unwrap_err();
    assert!(
        matches!(&error, Error::PasswordRequired { account, kind: DaemonKind::Task } if account == "alice"),
        "{error:?}"
    );
    assert_eq!(
        password_of(&user, || Ok(Some("secret".to_owned()))).unwrap().as_deref(),
        Some("secret")
    );
    assert!(password_of(&user, || Err(Error::InvalidPassword { variable: "V".into() })).is_err());
    assert_eq!(
        password_of(&root(), || panic!("a service account reads no password")).unwrap(),
        None
    );
}

/// Without its password, or without the logon right, a real account's task is not written, and nothing
/// of the install exists: not the task and not its record directory.
#[skuld::test]
fn a_real_account_needs_its_password_and_the_batch_right_before_any_write() {
    let (dir, runs) = records();
    let spec = task_spec_as(User::Name("alice".into()), Some("30 3 * * *"));
    let fake = Fake::new();

    let refused = scm(&fake).task_write_with(
        &spec,
        &definition(&spec),
        None,
        &runs,
        &mut Running(vec![]),
        |account| {
            password_of(account, || Ok(None)).map(|password| Prepared {
                principal: system(),
                password,
            })
        },
    );

    assert!(matches!(refused, Err(Error::PasswordRequired { .. })), "{refused:?}");
    assert_eq!(fake.calls(), []);
    assert!(!records_dir(&dir).exists());
}

/// The batch right is granted to the account before the task is registered (Decision 26): without
/// it a first start starts nothing and raises nothing. The closure stands for the grant, and sees the
/// scheduler untouched.
#[skuld::test]
fn install_of_a_real_user_task_grants_the_batch_right_before_the_first_write() {
    let (_dir, runs) = made_records();
    let spec = task_spec_as(User::Name("alice".into()), Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));
    let granted = std::cell::Cell::new(false);

    scm(&fake)
        .task_write_with(
            &spec,
            &definition(&spec),
            None,
            &runs,
            &mut Running(vec![]),
            |account| {
                assert_eq!(account, &TaskAccount::User("alice".to_owned()));
                assert_eq!(
                    fake.calls(),
                    [],
                    "the right is granted before the scheduler is asked anything"
                );
                granted.set(true);
                Ok(Prepared {
                    principal: process_user_sid(),
                    password: Some("secret".to_owned()),
                })
            },
        )
        .unwrap();

    assert!(granted.get());
    assert_eq!(
        fake.calls(),
        [register_call(
            &spec,
            false,
            TaskAccount::User("alice".to_owned()),
            Some("secret"),
            false,
            &registered_token(&fake)
        )]
    );
}

/// Everything the first write needs is reserved before it: a record directory that cannot be made
/// fails the install with the scheduler never asked.
#[skuld::test]
fn acquisition_precedes_the_first_register_call() {
    let file = tempfile::NamedTempFile::new().unwrap();
    // `%ProgramData%` that is a file: its `Goetia\runs` cannot be made.
    let runs = Runs::of(file.path(), &id());
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();

    let failed = scm(&fake).task_write_with(
        &spec,
        &definition(&spec),
        None,
        &runs,
        &mut Running(vec![]),
        prepared(system(), None),
    );

    assert!(failed.is_err());
    assert_eq!(fake.calls(), []);
}

#[skuld::test]
fn a_raced_create_and_a_raced_update_are_raced() {
    let spec = task_spec(Some("30 3 * * *"));
    let (_dir, runs) = made_records();
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Raced))
        .on_register(Ok(Registration::Raced));
    let scm = scm(&fake);

    // The test's own account, which a second write names again: the directory's permissions are set by
    // opening it, which an account that the first write left out has no right to.
    let created = scm
        .task_write_with(
            &spec,
            &definition(&spec),
            None,
            &runs,
            &mut Running(vec![]),
            prepared(process_user_sid(), None),
        )
        .unwrap();
    // An update, so the task that is there already runs as the same account.
    let as_me = task_spec_as(
        User::Id(AccountId::Sid(crate::testing::shown_sid(&process_user_sid()))),
        Some("30 3 * * *"),
    );
    let updated = scm
        .task_write_with(
            &as_me,
            &definition(&as_me),
            Some(&snapshot_of(&as_me, false)),
            &runs,
            &mut Running(vec![]),
            prepared(process_user_sid(), None),
        )
        .unwrap();

    assert!(matches!(created, WriteOutcome::Raced), "{created:?}");
    assert!(matches!(updated, WriteOutcome::Raced), "{updated:?}");
}

#[skuld::test]
fn a_failed_registration_is_the_installs_error() {
    let spec = task_spec(Some("30 3 * * *"));
    let (_dir, runs) = made_records();
    let fake = Fake::new();
    fake.on_register(Err(Error::os("register", std::io::Error::other("denied"))));

    let failed = scm(&fake).task_write_with(
        &spec,
        &definition(&spec),
        None,
        &runs,
        &mut Running(vec![]),
        prepared(system(), None),
    );

    assert!(failed.is_err());
}

// The well-known accounts' SIDs ---------------------------------------------------------------------------------------

#[skuld::test]
fn the_three_well_known_accounts_have_their_sids() {
    assert_eq!(well_known_sid("S-1-5-18").unwrap(), sid_from_parts(5, &[18]));
    assert_eq!(well_known_sid("S-1-5-19").unwrap(), sid_from_parts(5, &[19]));
    assert_eq!(well_known_sid("S-1-5-20").unwrap(), sid_from_parts(5, &[20]));
    for other in ["SYSTEM", "S-1-5-21", "S-1-5-18-1", "", "S-1-1-0"] {
        assert!(well_known_sid(other).is_err(), "{other:?}");
    }
}

// Enable and disable ==================================================================================================

/// What the installed XML holds beyond what goetia wrote: a hand edit of a value, which `enable` and
/// `disable` keep because they change the triggers and nothing else.
fn hand_edited(registered: &Registered) -> Registered {
    Registered {
        xml: registered
            .xml
            .replace("<Priority>5</Priority>", "<Priority>6</Priority>"),
        data: registered.data.clone(),
        enabled: registered.enabled,
    }
}

/// `disable` registers the stored XML with no triggers, and `enable` registers it with the triggers the
/// blob's schedule compiles to; each keeps a hand edit made elsewhere, and neither touches the task's own flag.
#[skuld::test]
fn disable_writes_no_triggers_and_enable_writes_the_compiled_ones_and_both_keep_a_hand_edit() {
    let spec = task_spec(Some("30 3 * * *"));
    let armed = hand_edited(&registered_task(&spec, true));
    assert!(armed.xml.contains("<Priority>6</Priority>"), "the edit is made");
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created))
        .on_register(Ok(Registration::Created));
    let scm = scm(&fake);
    let (_dir, runs) = records();

    scm.task_set_armed_with(
        &TaskSnapshot::new(id(), Ok(blob(&spec)), armed.clone()),
        false,
        &runs,
        || panic!("a service account reads no password"),
    )
    .unwrap();
    let calls = fake.calls();
    let Call::Register {
        xml: disarmed,
        who,
        update,
        ..
    } = &calls[0]
    else {
        panic!("disable registers: {calls:?}");
    };
    assert!(
        !disarmed.contains("<Triggers"),
        "disable writes no triggers: {disarmed}"
    );
    assert!(!TaskXml::armed(disarmed));
    assert!(
        disarmed.contains("<Priority>6</Priority>"),
        "a hand edit survives disable"
    );
    assert_eq!((who, update), (&root(), &true));

    let disabled = Registered {
        xml: disarmed.clone(),
        data: armed.data.clone(),
        enabled: armed.enabled,
    };
    scm.task_set_armed_with(&TaskSnapshot::new(id(), Ok(blob(&spec)), disabled), true, &runs, || {
        panic!("a service account reads no password")
    })
    .unwrap();
    let calls = fake.calls();
    let Call::Register {
        xml: rearmed, update, ..
    } = &calls[1]
    else {
        panic!("enable registers: {calls:?}");
    };
    assert!(TaskXml::armed(rearmed), "enable writes the triggers: {rearmed}");
    assert!(
        rearmed.contains("<Priority>6</Priority>"),
        "a hand edit survives enable"
    );
    assert!(update);
    assert_eq!(rearmed, &armed.xml, "enable writes back the armed task's stored XML");
    fake.finish();
}

/// `enable` of a task that has triggers keeps them as they are, a hand-added one included, and does not need the
/// blob: one that will not decode is no obstacle.
#[skuld::test]
fn enable_keeps_the_triggers_that_are_there_and_needs_no_blob_for_them() {
    let spec = task_spec(Some("30 3 * * *"));
    let armed = registered_task(&spec, true);
    let hand_added = Registered {
        xml: armed.xml.replace(
            "</Triggers>",
            "<LogonTrigger><UserId>S-1-5-18</UserId></LogonTrigger></Triggers>",
        ),
        data: armed.data.clone(),
        enabled: armed.enabled,
    };
    assert!(hand_added.xml.contains("LogonTrigger"), "the edit is made");
    // Off by its own flag, as an install or an uninstall that did not finish leaves it.
    let off = Registered {
        xml: hand_added
            .xml
            .replace("<Enabled>true</Enabled>", "<Enabled>false</Enabled>"),
        data: armed.data.clone(),
        enabled: Some(false),
    };
    for blob in [
        Ok(blob(&spec)),
        Err(std::sync::Arc::new(Error::blob("the blob will not decode"))),
    ] {
        let fake = Fake::new();
        fake.on_register(Ok(Registration::Created));
        let scm = scm(&fake);
        let (_dir, runs) = records();
        scm.task_set_armed_with(&TaskSnapshot::new(id(), blob, off.clone()), true, &runs, || Ok(None))
            .unwrap();
        let calls = fake.calls();
        let Call::Register { xml, .. } = &calls[0] else {
            panic!("enable registers: {calls:?}");
        };
        assert_eq!(xml, &hand_added.xml, "the triggers are kept, and the own flag is true");
        fake.finish();
    }
}

/// A blob that will not decode names no schedule, so `enable` has no triggers to write and says why; `disable`
/// needs none, and works.
#[skuld::test]
fn enable_of_a_task_whose_blob_will_not_decode_is_refused_before_a_write_and_disable_works() {
    let spec = task_spec(Some("30 3 * * *"));
    let armed = registered_task(&spec, true);
    let undecodable = |registered: Registered| {
        TaskSnapshot::new(
            id(),
            Err(std::sync::Arc::new(Error::blob("the blob will not decode"))),
            registered,
        )
    };
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));
    let scm = scm(&fake);
    let (_dir, runs) = records();

    scm.task_set_armed_with(&undecodable(armed.clone()), false, &runs, || Ok(None))
        .unwrap();
    let calls = fake.calls();
    let Call::Register { xml: disarmed, .. } = &calls[0] else {
        panic!("disable registers: {calls:?}");
    };
    assert!(!TaskXml::armed(disarmed));

    let disabled = Registered {
        xml: disarmed.clone(),
        data: armed.data.clone(),
        enabled: armed.enabled,
    };
    let refused = scm
        .task_set_armed_with(&undecodable(disabled), true, &runs, || Ok(None))
        .unwrap_err();
    assert!(refused.to_string().contains("will not decode"), "{refused}");
    assert_eq!(fake.calls().len(), 1, "nothing was sent");
    fake.finish();
}

#[skuld::test]
fn enable_of_a_task_with_no_schedule_is_refused_before_a_write() {
    let fake = Fake::new();
    let spec = task_spec(None);
    let installed = snapshot_of(&spec, false);

    let refused = crate::manager::flow::require_armable(&installed).unwrap_err();

    assert!(
        refused.to_string().contains(crate::manager::flow::NOTHING_TO_ARM),
        "{refused}"
    );
    assert_eq!(fake.calls(), []);
}

/// A schedule no month has (`30 2`) is a trigger that never fires, and a task with a trigger is armed
/// by `enable` like any other.
#[skuld::test]
fn enable_of_a_schedule_that_never_fires_arms_it() {
    let spec = task_spec(Some("0 0 30 2 *"));
    let installed = snapshot_of(&spec, false);
    assert!(!installed.armed());
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));
    let (_dir, runs) = records();

    scm(&fake)
        .task_set_armed_with(&installed, true, &runs, || {
            panic!("a service account reads no password")
        })
        .unwrap();

    let calls = fake.calls();
    let Call::Register { xml, .. } = &calls[0] else {
        panic!("enable registers: {calls:?}");
    };
    assert!(TaskXml::armed(xml), "{xml}");
}

/// Re-registering a password-logon task takes its password (M3): without it `enable` and `disable`
/// refuse, and the scheduler is not asked anything.
#[skuld::test]
fn enable_of_a_real_user_task_without_its_password_is_refused_before_any_write() {
    let spec = task_spec_as(User::Name("alice".into()), Some("30 3 * * *"));
    let fake = Fake::new();
    let (_dir, runs) = records();

    let refused = scm(&fake).task_set_armed_with(&snapshot_of(&spec, false), true, &runs, || Ok(None));

    assert!(
        matches!(&refused, Err(Error::PasswordRequired { account, kind: DaemonKind::Task }) if account == "alice"),
        "{refused:?}"
    );
    assert_eq!(fake.calls(), []);
}

#[skuld::test]
fn disable_of_a_real_user_task_without_its_password_is_refused_before_any_write() {
    let spec = task_spec_as(User::Name("alice".into()), Some("30 3 * * *"));
    let fake = Fake::new();
    let (_dir, runs) = records();

    let refused = scm(&fake).task_set_armed_with(&snapshot_of(&spec, true), false, &runs, || Ok(None));

    assert!(matches!(refused, Err(Error::PasswordRequired { .. })), "{refused:?}");
    assert_eq!(fake.calls(), []);
}

#[skuld::test]
fn enable_of_a_real_user_task_registers_it_again_with_its_password() {
    let spec = task_spec_as(User::Name("alice".into()), Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));
    let (_dir, runs) = records();

    scm(&fake)
        .task_set_armed_with(&snapshot_of(&spec, false), true, &runs, || Ok(Some("secret".to_owned())))
        .unwrap();

    let calls = fake.calls();
    let Call::Register {
        who, password, update, ..
    } = &calls[0]
    else {
        panic!("enable registers: {calls:?}");
    };
    assert_eq!(who, &TaskAccount::User("alice".to_owned()));
    assert_eq!(password.as_deref(), Some("secret"));
    assert!(update);
}

#[skuld::test]
fn enable_of_a_task_that_went_meanwhile_is_not_installed() {
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Raced));
    let (_dir, runs) = records();

    let failed = scm(&fake)
        .task_set_armed_with(&snapshot_of(&spec, false), true, &runs, || Ok(None))
        .unwrap_err();

    assert!(matches!(failed, Error::NotInstalled { id } if id == "nightly"));
}

/// A marker that stands means an install or an uninstall is running or did not finish, and only those two finish
/// it: `enable` and `disable` refuse before the password is asked for, send nothing to Task Scheduler and leave
/// the marker.
fn arming_is_refused_while_the_marker_stands(armed: bool) {
    let spec = task_spec(Some("30 3 * * *"));
    let (_dir, runs) = made_records();
    runs.close().unwrap();
    let fake = Fake::new();

    let refused = scm(&fake)
        .task_set_armed_with(&snapshot_of(&spec, !armed), armed, &runs, || {
            panic!("the refusal comes before the password is asked for")
        })
        .unwrap_err();

    let text = refused.report().to_string();
    assert!(
        text.contains("`nightly` is being, or did not finish being, installed or uninstalled")
            && text.contains("goetia daemon uninstall nightly"),
        "{text}"
    );
    assert_eq!(fake.calls(), [], "nothing was sent to Task Scheduler");
    assert!(runs.is_closed().unwrap(), "the marker still stands");
}

#[skuld::test]
fn enable_is_refused_while_the_marker_stands() {
    arming_is_refused_while_the_marker_stands(true);
}

#[skuld::test]
fn disable_is_refused_while_the_marker_stands() {
    arming_is_refused_while_the_marker_stands(false);
}

/// A task whose blob will not decode still has a principal, and a verb that re-registers it uses that.
#[skuld::test]
fn the_account_of_a_task_whose_blob_will_not_decode_is_its_stored_principal() {
    let spec = task_spec_as(User::Name("alice".into()), Some("30 3 * * *"));
    let registered = registered_task(&spec, false);
    assert_eq!(
        stored_account(&registered).unwrap(),
        TaskAccount::User("alice".to_owned())
    );

    let broken = Registered {
        xml: registered.xml.clone(),
        data: registered.data.replace("Spec: ", "Spec: !"),
        enabled: registered.enabled,
    };
    assert_eq!(
        stored_account(&broken).unwrap(),
        TaskAccount::User("alice".to_owned()),
        "the principal says it as the blob did"
    );
}

// Retype ==============================================================================================================

#[skuld::test]
fn a_running_daemon_is_not_retyped_and_a_stopped_one_is() {
    let id = id();
    for (old, text) in [
        (
            DaemonKind::Service,
            "`nightly` is running as a service, and changing it to a task removes it first: run `goetia daemon stop \
             nightly`, then re-run install",
        ),
        (
            DaemonKind::Task,
            "`nightly` is running as a task, and changing it to a service removes it first: run `goetia daemon stop \
             nightly`, then re-run install",
        ),
    ] {
        let refused = refuse_running(&id, old, State::Running).unwrap_err();
        assert_eq!(refused.to_string(), text);
        for state in [State::Idle, State::Stopped, State::Failed, State::Unknown] {
            assert!(refuse_running(&id, old, state).is_ok(), "{old:?} {state:?}");
        }
    }
}

/// The machine's processes for a retype's removal: the `alive` pids run, and none can be held (so a
/// removal that meets one has failed the test).
struct Running(Vec<u32>);

impl Processes for Running {
    type Held = ();

    fn alive(&mut self, pid: u32) -> std::io::Result<Liveness> {
        Ok(if self.0.contains(&pid) {
            Liveness::Alive
        } else {
            Liveness::Gone
        })
    }

    fn open(&mut self, pid: u32) -> std::io::Result<Option<()>> {
        unreachable!("a retype stops nothing: pid {pid} was held")
    }

    fn terminate(&mut self, _: &()) -> std::io::Result<()> {
        unreachable!("a retype stops nothing")
    }

    fn await_exit(
        &mut self,
        _: &(),
        _: crate::manager::budget::Deadline,
    ) -> std::io::Result<crate::backend::kill::Waited> {
        unreachable!("a retype stops nothing")
    }
}

fn running_run() -> Instance {
    Instance {
        id: InstanceId::parse(&format!("{:032x}", 1)).unwrap(),
        pid: 4242,
    }
}

/// A task that runs is refused before anything is acquired or removed, with the exact sentence.
#[skuld::test]
fn a_task_that_runs_is_not_changed_into_a_service() {
    let (_dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_instances(Ok(vec![running_run()]));

    let refused = scm(&fake)
        .retype_to_service_with(
            &snapshot_of(&spec, false),
            &runs,
            &mut Running(vec![4242]),
            || -> Result<()> { panic!("nothing is acquired for a retype that is refused") },
            |()| -> Result<WriteOutcome> { panic!("no service is created") },
        )
        .unwrap_err();

    assert!(
        matches!(
            refused,
            Error::RetypeRunning {
                old: DaemonKind::Task,
                ..
            }
        ),
        "{refused:?}"
    );
    assert_eq!(
        fake.calls(),
        [Call::Instances(PATH.to_owned())],
        "the task is untouched"
    );
}

/// What the service needs (its account, password, logon right, event source) is acquired before the
/// task is touched: a gate that fails leaves the task as it was, with the scheduler asked only whether it
/// runs.
#[skuld::test]
fn a_gate_that_fails_leaves_the_task_untouched() {
    let (dir, runs) = made_records();
    runs.ensure(&[process_user_sid()]).unwrap();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_instances(Ok(Vec::new()));

    let refused = scm(&fake)
        .retype_to_service_with(
            &snapshot_of(&spec, true),
            &runs,
            &mut Running(vec![]),
            || -> Result<()> {
                Err(Error::PasswordRequired {
                    account: "alice".to_owned(),
                    kind: DaemonKind::Service,
                })
            },
            |()| -> Result<WriteOutcome> { panic!("no service is created") },
        )
        .unwrap_err();

    assert!(matches!(refused, Error::PasswordRequired { .. }), "{refused:?}");
    assert_eq!(
        fake.calls(),
        [Call::Instances(PATH.to_owned())],
        "nothing was disabled or deleted"
    );
    assert!(records_dir(&dir).is_dir(), "its records are there");
}

#[skuld::test]
fn a_task_is_removed_after_the_gate_and_before_the_service_is_created() {
    let (dir, runs) = made_records();
    runs.ensure(&[process_user_sid()]).unwrap();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_instances(Ok(Vec::new()))
        .on_instances(Ok(Vec::new()))
        .on_set_enabled(Ok(()))
        .on_delete(Ok(true));
    let scm = scm(&fake);
    let order = std::cell::RefCell::new(Vec::new());

    let wrote = scm
        .retype_to_service_with(
            &snapshot_of(&spec, true),
            &runs,
            &mut Running(vec![]),
            || {
                assert_eq!(
                    fake.calls(),
                    [Call::Instances(PATH.to_owned())],
                    "only the refusal came first"
                );
                order.borrow_mut().push("gate");
                Ok("password")
            },
            |gated| {
                assert_eq!(gated, "password", "what the gate returned reaches the creation");
                assert!(
                    matches!(fake.calls().last(), Some(Call::Delete(_))),
                    "the task is deleted before the service is created"
                );
                assert!(!records_dir(&dir).exists(), "and its records are gone");
                order.borrow_mut().push("create");
                Ok(WriteOutcome::Done)
            },
        )
        .unwrap();

    assert!(matches!(wrote, WriteOutcome::Done));
    assert_eq!(*order.borrow(), ["gate", "create"]);
    fake.finish();
}

// ... and a service into a task. The service is the real SCM's, so its refusal and its removal are the
// closures the real retype passes, and the order is the one that counts.

#[skuld::test]
fn a_service_that_runs_is_not_changed_into_a_task() {
    let (dir, runs) = records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();

    let refused = scm(&fake)
        .retype_to_task_with(
            &spec,
            &definition(&spec),
            &runs,
            |_| panic!("nothing is acquired for a retype that is refused"),
            || refuse_running(&id(), DaemonKind::Service, State::Running).map(|()| None),
            || panic!("the service is not removed"),
        )
        .unwrap_err();

    assert!(
        matches!(
            refused,
            Error::RetypeRunning {
                old: DaemonKind::Service,
                ..
            }
        ),
        "{refused:?}"
    );
    assert_eq!(fake.calls(), []);
    assert!(!records_dir(&dir).exists());
}

/// A real account's password, logon right and the record directory are acquired before the service is
/// removed: a gate that fails leaves the service installed.
#[skuld::test]
fn a_gate_that_fails_leaves_the_service_untouched() {
    let (dir, runs) = records();
    let spec = task_spec_as(User::Name("alice".into()), Some("30 3 * * *"));
    let fake = Fake::new();

    let refused = scm(&fake)
        .retype_to_task_with(
            &spec,
            &definition(&spec),
            &runs,
            |account| {
                password_of(account, || Ok(None)).map(|password| Prepared {
                    principal: system(),
                    password,
                })
            },
            || Ok(None),
            || panic!("the service is not removed when what the task needs is missing"),
        )
        .unwrap_err();

    assert!(matches!(refused, Error::PasswordRequired { .. }), "{refused:?}");
    assert_eq!(fake.calls(), []);
    assert!(!records_dir(&dir).exists(), "not even the record directory is made");

    // And a record directory that cannot be made is the same.
    let file = tempfile::NamedTempFile::new().unwrap();
    let blocked = Runs::of(file.path(), &id());
    let refused = scm(&fake).retype_to_task_with(
        &spec,
        &definition(&spec),
        &blocked,
        prepared_alice(),
        || Ok(None),
        || panic!("the service is not removed when its record directory cannot be made"),
    );
    assert!(refused.is_err());
}

fn prepared_alice() -> impl FnOnce(&TaskAccount) -> Result<Prepared> {
    prepared(process_user_sid(), Some("secret"))
}

/// A service changed into a task names no account beside the task's in the record directory (the service never
/// wrote there), so the landing leaves the directory as the write made it: the task's account alone.
/// Setting the permissions of a directory that names an account other than the test's needs an elevated token.
#[skuld::test]
fn a_service_changed_into_a_task_names_the_task_account_alone_in_the_record_directory(
    #[fixture(scm_group)] _group: &Group,
) {
    let (dir, runs) = made_records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));

    scm(&fake)
        .retype_to_task_with(
            &spec,
            &definition(&spec),
            &runs,
            prepared(process_user_sid(), None),
            || Ok(Some(local_service())),
            || Ok(()),
        )
        .unwrap();

    let trustees = trustees_of(&records_dir(&dir));
    assert!(trustees.contains(&process_user_sid()), "the task's account");
    assert!(!trustees.contains(&local_service()), "and not the service's");
}

#[skuld::test]
fn a_service_is_removed_after_the_task_is_acquired_and_before_it_is_registered() {
    let (_dir, runs) = made_records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));
    let retired = std::cell::Cell::new(false);

    let wrote = scm(&fake)
        .retype_to_task_with(
            &spec,
            &definition(&spec),
            &runs,
            prepared(process_user_sid(), None),
            || Ok(None),
            || {
                assert_eq!(
                    fake.calls(),
                    [],
                    "the scheduler is asked nothing before the service goes"
                );
                retired.set(true);
                Ok(())
            },
        )
        .unwrap();

    assert!(matches!(wrote, WriteOutcome::Done), "{wrote:?}");
    assert!(retired.get());
    assert_eq!(
        fake.calls(),
        [register_call(
            &spec,
            false,
            root(),
            None,
            false,
            &registered_token(&fake)
        )],
        "a create, disarmed"
    );
    fake.finish();
}

/// A service changed into a task never leaves what is registered without its log. The service's account and the
/// task's are named before the service is retired, and the task's alone once the task is registered: a retirement
/// that fails leaves the service, and a registration that fails or is raced leaves the service gone and the
/// task's account to be, and either way both accounts may append until an install that lands corrects it. What
/// the file held is not read: an entry no install names is gone.
#[skuld::test]
fn a_service_changed_into_a_task_that_does_not_land_leaves_both_accounts_their_log() {
    let spec = task_spec(Some("30 3 * * *"));
    for step in ["retire", "register", "raced"] {
        let (dir, runs) = made_records();
        log_with_an_entry_no_install_names(&dir);
        let fake = Fake::new();
        match step {
            "register" => {
                fake.on_register(Err(Error::os("register", std::io::Error::other("denied"))));
            }
            "raced" => {
                fake.on_register(Ok(Registration::Raced));
            }
            _ => {}
        }

        let _ = scm(&fake).retype_to_task_with(
            &spec,
            &definition(&spec),
            &runs,
            prepared(users(), None),
            || Ok(Some(local_service())),
            || {
                if step == "retire" {
                    Err(Error::os("retire", std::io::Error::other("denied")))
                } else {
                    Ok(())
                }
            },
        );

        let trustees = trustees_of(&default_log(&dir));
        assert!(
            trustees.contains(&local_service()),
            "{step}: the service's account keeps its log"
        );
        assert!(trustees.contains(&users()), "{step}: the task's account has its own");
        assert!(
            !trustees.contains(&process_user_sid()),
            "{step}: an entry nothing names was carried over"
        );
    }
}

/// Both accounts are named when the service is retired, and the task's alone once the task is registered.
#[skuld::test]
fn a_service_changed_into_a_task_leaves_the_tasks_account_alone_in_the_log_once_it_is_registered() {
    let spec = task_spec(Some("30 3 * * *"));
    let (dir, runs) = made_records();
    log_with_an_entry_no_install_names(&dir);
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));

    let wrote = scm(&fake)
        .retype_to_task_with(
            &spec,
            &definition(&spec),
            &runs,
            prepared(users(), None),
            || Ok(Some(local_service())),
            || {
                let trustees = trustees_of(&default_log(&dir));
                assert!(
                    trustees.contains(&local_service()) && trustees.contains(&users()),
                    "both accounts are named when the service is retired"
                );
                Ok(())
            },
        )
        .unwrap();

    assert!(matches!(wrote, WriteOutcome::Done), "{wrote:?}");
    assert_names_the_task_alone(&dir);
    fake.finish();
}

/// A service with no account to name (its account is gone, or it is not a hosted one) adds no entry: the log
/// names the task's account alone from the start.
#[skuld::test]
fn a_service_with_no_account_to_name_adds_none_to_the_log() {
    let spec = task_spec(Some("30 3 * * *"));
    let (dir, runs) = made_records();
    log_with_an_entry_no_install_names(&dir);
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));

    scm(&fake)
        .retype_to_task_with(
            &spec,
            &definition(&spec),
            &runs,
            prepared(users(), None),
            || Ok(None),
            || {
                assert_eq!(
                    trustees_of(&default_log(&dir))
                        .iter()
                        .filter(|t| **t == users())
                        .count(),
                    1
                );
                assert!(!trustees_of(&default_log(&dir)).contains(&process_user_sid()));
                Ok(())
            },
        )
        .unwrap();

    assert_names_the_task_alone(&dir);
    fake.finish();
}

/// A snapshot of the service `frpc` of `protocol` that is registered as `account` (`None`: LocalSystem), as
/// `read` makes one, with its configuration queried already when `queried`: a service that is not hosted has
/// none, so a snapshot without it shows that nothing asked for it.
fn service_registered_as(protocol: crate::spec::Protocol, account: Option<&str>, queried: bool) -> ScmSnapshot {
    use windows_service::service::{ServiceConfig, ServiceErrorControl, ServiceStartType, ServiceType};

    let blob = crate::backend::scm::test_support::blob_of(protocol);
    let snapshot = ScmSnapshot::new(blob.spec.common().id.clone(), Ok(blob), Default::default());
    if queried {
        let config = ServiceConfig {
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::OnDemand,
            error_control: ServiceErrorControl::Normal,
            executable_path: PathBuf::from("host.exe"),
            load_order_group: None,
            tag_id: 0,
            dependencies: vec![],
            account_name: account.map(Into::into),
            display_name: "frpc".into(),
        };
        assert!(snapshot.config.set(config).is_ok());
    }
    snapshot
}

const LOCAL_SERVICE_NAME: &str = r"NT AUTHORITY\LocalService";
const NO_SUCH_ACCOUNT: &str = "goetia-no-such-account-7f3a";

/// A native service has no default log, so there is no account of it to name, and its configuration is not
/// asked for.
#[skuld::test]
fn a_native_service_has_no_account_the_log_keeps() {
    let native = service_registered_as(crate::spec::Protocol::Native, Some(LOCAL_SERVICE_NAME), false);

    let kept = kept_service_account(&native, &TaskAccount::User(NO_SUCH_ACCOUNT.to_string()));

    assert_eq!(kept.unwrap(), None);
}

/// A service whose account no longer exists (deleted since) has no one to name: nothing runs as it, and the
/// retype goes on.
#[skuld::test]
fn a_service_whose_account_is_gone_has_no_account_the_log_keeps() {
    let gone = service_registered_as(crate::spec::Protocol::Plain, Some(NO_SUCH_ACCOUNT), true);

    let kept = kept_service_account(&gone, &TaskAccount::Service("S-1-5-19".to_string()));

    assert_eq!(kept.unwrap(), None);
}

/// A service that runs as the task's account has no other account to keep, and nothing is looked up for it:
/// the account a lookup would find is the task's.
#[skuld::test]
fn a_service_that_runs_as_the_tasks_account_has_no_account_the_log_keeps() {
    for (registered, task) in [
        (Some(LOCAL_SERVICE_NAME), TaskAccount::Service("S-1-5-19".to_string())),
        (None, TaskAccount::Service("S-1-5-18".to_string())),
        (Some(NO_SUCH_ACCOUNT), TaskAccount::User(NO_SUCH_ACCOUNT.to_string())),
    ] {
        let service = service_registered_as(crate::spec::Protocol::Plain, registered, true);

        let kept = kept_service_account(&service, &task);

        assert_eq!(kept.unwrap(), None, "{registered:?} for {task:?}");
    }
}

/// A hosted service that runs as another account than the task's keeps its account in the log until the retype
/// lands.
#[skuld::test]
fn a_service_that_runs_as_another_account_than_the_tasks_has_its_account_kept() {
    let service = service_registered_as(crate::spec::Protocol::Plain, Some(LOCAL_SERVICE_NAME), true);

    let kept = kept_service_account(&service, &TaskAccount::Service("S-1-5-20".to_string()));

    assert_eq!(kept.unwrap(), Some(local_service()));
}

/// The task's account as a service's registration spells it, which is what the service's own is compared with.
#[skuld::test]
fn a_task_account_is_spelled_as_a_service_registers_it() {
    assert_eq!(
        service_account_name(&TaskAccount::Service("S-1-5-18".to_string())),
        None
    );
    assert_eq!(
        service_account_name(&TaskAccount::Service("S-1-5-19".to_string())).as_deref(),
        Some(LOCAL_SERVICE_NAME)
    );
    assert_eq!(
        service_account_name(&TaskAccount::Service("S-1-5-20".to_string())).as_deref(),
        Some(r"NT AUTHORITY\NetworkService")
    );
    assert_eq!(
        service_account_name(&TaskAccount::User(r"DOMAIN\alice".to_string())).as_deref(),
        Some(r"DOMAIN\alice")
    );
}

/// The default log in `dir` names the task's account, and neither the service's nor an entry nothing names.
#[track_caller]
fn assert_names_the_task_alone(dir: &tempfile::TempDir) {
    let trustees = trustees_of(&default_log(dir));
    assert!(trustees.contains(&users()));
    assert!(!trustees.contains(&local_service()));
    assert!(
        !trustees.contains(&process_user_sid()),
        "an entry nothing names was carried"
    );
}
