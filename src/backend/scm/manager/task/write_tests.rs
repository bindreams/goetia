//! The write half of the task side over the table-driven fake of Task Scheduler, with the run records
//! in a temporary directory. What the machine must hold before a task is written (its account, its
//! logon right, the event source) is the tests' own closure, so nothing here changes the machine.

use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::PathBuf;

use tempfile::TempDir;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, READ_CONTROL};

use super::*;
use crate::backend::scm::instance::InstanceId;
use crate::backend::scm::manager::task::api::{Instance, Registration};
use crate::backend::scm::manager::task::fake::{Call, Fake};
use crate::backend::scm::shim::acl::read_object;
use crate::backend::scm::sid::{Sid, sid_from_parts, system};
use crate::backend::scm::test_support::{registered_task, task_host, task_spec, task_spec_as};
use crate::blob::Blob;
use crate::decide::{self, ExternalConfig, Ownership};
use crate::elevation::test_support::process_user_sid;
use crate::manager::State;
use crate::spec::{DaemonKind, User};

fn id() -> Id {
    Id::try_from("nightly").unwrap()
}

const PATH: &str = r"\Goetia\nightly";

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

/// [`records`] with the task's directory made plainly, for a write that gets as far as making it. Making
/// it as an install does names Administrators as its owner, which needs an elevated token (a test of the
/// `scm` group makes it); a directory that is there has only its permissions set.
fn made_records() -> (TempDir, Runs) {
    let (dir, runs) = records();
    std::fs::create_dir_all(records_dir(&dir)).unwrap();
    (dir, runs)
}

fn records_dir(dir: &TempDir) -> PathBuf {
    dir.path().join("Goetia").join("runs").join("nightly")
}

/// What the machine holds for a task that runs as `principal`, with the password given.
fn prepared(principal: Sid, password: Option<&str>) -> impl FnOnce(&TaskAccount) -> Result<Prepared> {
    let password = password.map(str::to_owned);
    move |_| Ok(Prepared { principal, password })
}

fn register_call(spec: &DaemonSpec, armed: bool, who: TaskAccount, password: Option<&str>, update: bool) -> Call {
    Call::Register {
        path: PATH.to_owned(),
        xml: definition(spec).to_xml(armed),
        who,
        password: password.map(str::to_owned),
        update,
    }
}

fn root() -> TaskAccount {
    TaskAccount::Service("S-1-5-18".to_owned())
}

// Install =============================================================================================================

#[skuld::test]
fn install_of_a_task_creates_it_disarmed() {
    let (_dir, runs) = made_records();
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));

    let outcome = scm(&fake)
        .task_write_with(&spec, &definition(&spec), None, &runs, prepared(system(), None))
        .unwrap();

    assert!(matches!(outcome, WriteOutcome::Done), "{outcome:?}");
    assert_eq!(fake.calls(), [register_call(&spec, false, root(), None, false)]);
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
                prepared(system(), None),
            )
            .unwrap();

        assert!(matches!(outcome, WriteOutcome::Done), "{outcome:?}");
        assert_eq!(
            fake.calls(),
            [register_call(&spec, armed, root(), None, true)],
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
        let installed = snapshot_of(&spec, armed);
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
            prepared(system(), None),
        )
        .unwrap();

    assert_eq!(fake.calls(), [register_call(&rescheduled, false, root(), None, true)]);
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
/// records: the directory's permissions are rewritten by every write, an update included.
#[skuld::test]
fn reinstall_with_another_user_rewrites_the_record_directorys_dacl() {
    let (dir, runs) = made_records();
    let spec = task_spec(Some("30 3 * * *"));
    let me = process_user_sid();
    // `Users` has this test's account among its members, so the directory is still the test's to remove.
    let users = sid_from_parts(5, &[32, 545]);
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created))
        .on_register(Ok(Registration::Created));
    let scm = scm(&fake);
    scm.task_write_with(&spec, &definition(&spec), None, &runs, prepared(me.clone(), None))
        .unwrap();
    assert!(trustees_of(&records_dir(&dir)).contains(&me));

    scm.task_write_with(
        &spec,
        &definition(&spec),
        Some(&snapshot_of(&spec, false)),
        &runs,
        prepared(users.clone(), None),
    )
    .unwrap();

    let trustees = trustees_of(&records_dir(&dir));
    assert!(trustees.contains(&users), "the new account is named");
    assert!(!trustees.contains(&me), "the old account is not");
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

    let refused = scm(&fake).task_write_with(&spec, &definition(&spec), None, &runs, |account| {
        password_of(account, || Ok(None)).map(|password| Prepared {
            principal: system(),
            password,
        })
    });

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
        .task_write_with(&spec, &definition(&spec), None, &runs, |account| {
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
        })
        .unwrap();

    assert!(granted.get());
    assert_eq!(
        fake.calls(),
        [register_call(
            &spec,
            false,
            TaskAccount::User("alice".to_owned()),
            Some("secret"),
            false
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

    let failed = scm(&fake).task_write_with(&spec, &definition(&spec), None, &runs, prepared(system(), None));

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
            prepared(process_user_sid(), None),
        )
        .unwrap();
    let updated = scm
        .task_write_with(
            &spec,
            &definition(&spec),
            Some(&snapshot_of(&spec, false)),
            &runs,
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

    let failed = scm(&fake).task_write_with(&spec, &definition(&spec), None, &runs, prepared(system(), None));

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
/// `disable` keep because they change the flags and nothing else.
fn hand_edited(registered: &Registered) -> Registered {
    Registered {
        xml: registered
            .xml
            .replace("<Priority>5</Priority>", "<Priority>6</Priority>"),
        data: registered.data.clone(),
    }
}

#[skuld::test]
fn enable_then_disable_flip_only_the_flags_and_keep_a_hand_edit() {
    let spec = task_spec(Some("30 3 * * *"));
    let registered = hand_edited(&registered_task(&spec, false));
    assert!(registered.xml.contains("<Priority>6</Priority>"), "the edit is made");
    let installed = TaskSnapshot::new(id(), Ok(blob(&spec)), registered.clone());
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created))
        .on_register(Ok(Registration::Created));
    let scm = scm(&fake);

    scm.task_set_armed_with(&installed, true, || panic!("a service account reads no password"))
        .unwrap();
    let armed = Registered {
        xml: TaskXml::with_armed(&registered.xml, true).unwrap(),
        data: registered.data.clone(),
    };
    scm.task_set_armed_with(&TaskSnapshot::new(id(), Ok(blob(&spec)), armed.clone()), false, || {
        panic!("a service account reads no password")
    })
    .unwrap();

    let calls = fake.calls();
    let Call::Register { xml, who, update, .. } = &calls[0] else {
        panic!("enable registers: {calls:?}");
    };
    assert_eq!(xml, &armed.xml, "enable flips the flags and nothing else");
    assert!(TaskXml::armed(xml));
    assert!(xml.contains("<Priority>6</Priority>"), "a hand edit survives enable");
    assert_eq!((who, update), (&root(), &true));
    let Call::Register { xml, .. } = &calls[1] else {
        panic!("disable registers: {calls:?}");
    };
    assert_eq!(xml, &registered.xml, "disable puts the flags back");
    assert!(xml.contains("<Priority>6</Priority>"), "a hand edit survives disable");
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

    scm(&fake)
        .task_set_armed_with(&installed, true, || panic!("a service account reads no password"))
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

    let refused = scm(&fake).task_set_armed_with(&snapshot_of(&spec, false), true, || Ok(None));

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

    let refused = scm(&fake).task_set_armed_with(&snapshot_of(&spec, true), false, || Ok(None));

    assert!(matches!(refused, Err(Error::PasswordRequired { .. })), "{refused:?}");
    assert_eq!(fake.calls(), []);
}

#[skuld::test]
fn enable_of_a_real_user_task_registers_it_again_with_its_password() {
    let spec = task_spec_as(User::Name("alice".into()), Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_register(Ok(Registration::Created));

    scm(&fake)
        .task_set_armed_with(&snapshot_of(&spec, false), true, || Ok(Some("secret".to_owned())))
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

    let failed = scm(&fake)
        .task_set_armed_with(&snapshot_of(&spec, false), true, || Ok(None))
        .unwrap_err();

    assert!(matches!(failed, Error::NotInstalled { id } if id == "nightly"));
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
    runs.ensure(&process_user_sid()).unwrap();
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
    runs.ensure(&process_user_sid()).unwrap();
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
            || refuse_running(&id(), DaemonKind::Service, State::Running),
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
            || Ok(()),
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
        || Ok(()),
        || panic!("the service is not removed when its record directory cannot be made"),
    );
    assert!(refused.is_err());
}

fn prepared_alice() -> impl FnOnce(&TaskAccount) -> Result<Prepared> {
    prepared(process_user_sid(), Some("secret"))
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
            || Ok(()),
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
        [register_call(&spec, false, root(), None, false)],
        "a create, disarmed"
    );
    fake.finish();
}
