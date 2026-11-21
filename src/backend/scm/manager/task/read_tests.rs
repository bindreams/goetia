//! The read side of the task half over the table-driven fake of Task Scheduler: no COM, and no SCM
//! except for the service entries a test makes up.

use std::io;
use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::backend::scm::instance::InstanceId;
use crate::backend::scm::manager::task::api::{Instance, Registered, TaskInfo};
use crate::backend::scm::manager::task::fake::{Call, Fake};
use crate::backend::scm::parameters::Parameters;
use crate::backend::scm::test_support::{blob_of, registered_task, task_host, task_spec, task_spec_as};
use crate::blob::Blob;
use crate::decide::{self, ExternalConfig, Ownership};
use crate::error::Error;
use crate::manager::{Exit, Installed, State, Status};
use crate::spec::{AccountId, DaemonSpec, Protocol, User};
use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;

fn id() -> Id {
    Id::try_from("nightly").unwrap()
}

fn scm(fake: &Fake) -> Scm {
    Scm::new().with_tasks(Box::new(fake.clone()))
}

fn blob(spec: &DaemonSpec) -> Blob {
    Blob {
        schema: crate::blob::SCHEMA,
        version: crate::version().to_string(),
        spec: spec.clone(),
    }
}

/// The snapshot `read` makes of the task `nightly` that Task Scheduler holds disarmed.
fn snapshot() -> TaskSnapshot {
    let spec = task_spec(Some("30 3 * * *"));
    TaskSnapshot::new(id(), Ok(blob(&spec)), registered_task(&spec, false))
}

fn instance(n: u8, pid: u32) -> Instance {
    Instance {
        id: InstanceId::parse(&format!("{n:032x}")).unwrap(),
        pid,
    }
}

fn never_run() -> TaskInfo {
    TaskInfo {
        last_run: None,
        last_result: 0x0004_1303,
        next_run: None,
    }
}

fn alive(_pid: u32) -> io::Result<Liveness> {
    Ok(Liveness::Alive)
}

fn gone(_pid: u32) -> io::Result<Liveness> {
    Ok(Liveness::Gone)
}

fn task_status_of(status: Status) -> crate::manager::TaskStatus {
    match status {
        Status::Task(status) => status,
        Status::Service(_) => panic!("a task's status is a task's"),
    }
}

// Reading one task ====================================================================================================

#[skuld::test]
fn a_task_reads_as_ours_and_its_status_is_idle() {
    let spec = task_spec(Some("30 3 * * *"));
    let fake = Fake::new();
    fake.on_get(Ok(Some(registered_task(&spec, false))));
    let scm = scm(&fake);

    let ReadOutcome::Marked(snapshot) = scm.task_read(&id()).unwrap() else {
        panic!("a task with goetia's data is marked");
    };
    // As `judge` reads it: what is installed is what the spec regenerates, so the task is up to date.
    let installed = scm.task_installed(&snapshot).unwrap();
    let regenerated = scm.task_regenerate(&spec, &snapshot).unwrap();
    assert_eq!(installed.definition.to_string(), regenerated.to_string());

    fake.on_instances(Ok(Vec::new())).on_info(Ok(never_run()));
    let status = task_status_of(scm.task_status_with(&snapshot, alive).unwrap());

    assert_eq!(status.common.state, State::Idle);
    assert_eq!(status.common.pid, None);
    assert!(!status.common.enabled, "a task is installed disarmed");
    assert_eq!(status.last_result, None);
    assert_eq!(status.last_run, None);
    assert_eq!(status.next_run, None);
    fake.finish();
}

/// Only an account the system says does not exist is "no account known": any other failure of the lookup (a
/// transient RPC or domain-controller error) is the error, so that it is no licence to delete the task.
#[skuld::test]
fn a_failed_lookup_of_the_principal_is_a_change_only_when_the_account_does_not_exist() {
    let sid = crate::backend::scm::sid::system();
    assert_eq!(known_account(Ok(sid.clone())).unwrap(), Some(sid));

    let none_mapped = Error::AccountLookup {
        detail: "cannot resolve SID for account `gone`".to_owned(),
        source: None,
    };
    assert_eq!(known_account(Err(none_mapped)).unwrap(), None);

    let transient = Error::os(
        "resolve SID for account `alice`",
        io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32),
    );
    let failed = known_account(Err(transient)).unwrap_err();
    assert!(
        std::error::Error::source(&failed).is_some(),
        "the lookup's own error is kept: {failed:?}"
    );
}

#[skuld::test]
fn a_task_that_is_armed_reports_enabled_and_its_next_run() {
    let spec = task_spec(Some("30 3 * * *"));
    let snapshot = TaskSnapshot::new(id(), Ok(blob(&spec)), registered_task(&spec, true));
    let next = UNIX_EPOCH + Duration::from_secs(5_000);
    let fake = Fake::new();
    fake.on_instances(Ok(Vec::new())).on_info(Ok(TaskInfo {
        last_run: None,
        last_result: 0x0004_1303,
        next_run: Some(next),
    }));

    let status = task_status_of(scm(&fake).task_status_with(&snapshot, alive).unwrap());

    assert!(status.common.enabled);
    assert_eq!(status.next_run, Some(next));
}

#[skuld::test]
fn the_host_a_task_is_regenerated_with_is_the_installed_actions_not_a_providers() {
    let spec = task_spec(Some("30 3 * * *"));
    let moved = crate::shim::HostCommand {
        exe: crate::spec::test_support::abs("elsewhere/other-host.exe"),
        args: vec!["--other".to_owned()],
    };
    let definition = crate::backend::scm::task_xml::TaskXml::new(&spec, moved.clone()).unwrap();
    let registered = Registered {
        xml: definition.to_xml(false, &crate::backend::scm::test_support::task_token()),
        data: crate::blob::metadata_text(&spec),
        enabled: Some(true),
    };
    let snapshot = TaskSnapshot::new(id(), Ok(blob(&spec)), registered);

    // `Scm::new()` has no provider: a regeneration that asked for a host would fail.
    let regenerated = Scm::new().task_regenerate(&spec, &snapshot).unwrap();

    let crate::backend::scm::generate::WindowsDefinition::Task(task) = regenerated else {
        panic!("a task regenerated as {regenerated:?}");
    };
    assert_eq!(task.host(), moved);
}

#[skuld::test]
fn a_task_whose_xml_cannot_be_read_is_unreadable_not_a_bad_spec() {
    let spec = task_spec(Some("30 3 * * *"));
    let snapshot = TaskSnapshot::new(
        id(),
        Ok(blob(&spec)),
        Registered {
            xml: "<Task><Actions>".to_owned(),
            data: crate::blob::metadata_text(&spec),
            enabled: Some(true),
        },
    );
    let scm = Scm::new();

    assert!(scm.task_installed(&snapshot).is_err());
    let error = scm.task_regenerate(&spec, &snapshot).unwrap_err();
    assert!(error.wraps_unreadable_host(), "{error:?}");
}

// The account the installed definition shows --------------------------------------------------------------------------

/// `alice`'s SID before and after the account is created again.
const OLD_SID: &str = "S-1-5-21-1-2-3-1001";
const NEW_SID: &str = "S-1-5-21-1-2-3-1002";

fn sid_text(text: &str) -> Sid {
    let parts: Vec<u32> = text.split('-').skip(3).map(|part| part.parse().unwrap()).collect();
    crate::backend::scm::sid::sid_from_parts(5, &parts)
}

/// A lookup that answers from `known`, and says "no such account" for any other name.
fn knowing(known: &'static [(&'static str, &'static str)]) -> impl Fn(&str, Option<Written<'_>>) -> Result<Sid> {
    move |name, _| {
        known
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, sid)| sid_text(sid))
            .ok_or_else(|| Error::AccountLookup {
                detail: format!("cannot resolve SID for account `{name}`"),
                source: None,
            })
    }
}

fn denied(name: &str) -> Error {
    Error::os(
        format!("resolve SID for account `{name}`"),
        io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32),
    )
}

/// The task `nightly` of the user `alice`, as Task Scheduler stores it when its principal is the SID `stored`.
fn alices(stored: &str) -> (DaemonSpec, TaskSnapshot) {
    let spec = task_spec_as(User::Name("alice".to_owned()), Some("30 3 * * *"));
    let mut registered = registered_task(&spec, false);
    registered.xml = registered
        .xml
        .replace("<UserId>alice</UserId>", &format!("<UserId>{stored}</UserId>"));
    assert!(registered.xml.contains(stored), "the principal is the stored SID");
    let snapshot = TaskSnapshot::new(id(), Ok(blob(&spec)), registered);
    (spec, snapshot)
}

/// What an install of `spec` over `snapshot` decides, the definition read with `lookup`.
fn install_over(
    spec: &DaemonSpec,
    snapshot: &TaskSnapshot,
    lookup: impl Fn(&str, Option<Written<'_>>) -> Result<Sid>,
) -> (String, decide::Outcome) {
    let scm = Scm::new();
    let on_disk = scm
        .task_installed_with(snapshot, lookup)
        .unwrap()
        .definition
        .to_string();
    let regenerated = scm.task_regenerate(spec, snapshot).unwrap().to_string();
    let desired = TaskXml::new(spec, task_host()).unwrap().to_string();
    let outcome = decide::decide(
        &Ownership::Ours {
            blob: blob(spec),
            regenerated,
            on_disk: on_disk.clone(),
        },
        &desired,
        spec,
        crate::version(),
        false,
        &ExternalConfig::Absent,
    );
    (on_disk, outcome)
}

/// The registration names the account the blob's name names now, so the definition shows the blob's account and
/// an install of it is up to date.
#[skuld::test]
fn a_registration_for_the_account_the_name_names_now_shows_the_blobs_account() {
    let (spec, snapshot) = alices(OLD_SID);

    let (shown, outcome) = install_over(&spec, &snapshot, knowing(&[("alice", OLD_SID), (OLD_SID, OLD_SID)]));

    assert!(shown.contains(r#"Account: user "alice""#), "{shown}");
    assert!(matches!(outcome, decide::Outcome::UpToDate), "{outcome:?}");
}

/// The user was deleted and created again under the same name: the registration names the old account's SID, so
/// the definition shows it, and an install of the same spec is a conflict that names it.
#[skuld::test]
fn a_registration_for_another_account_than_the_name_names_now_shows_its_sid_and_conflicts() {
    let (spec, snapshot) = alices(OLD_SID);

    let (shown, outcome) = install_over(&spec, &snapshot, knowing(&[("alice", NEW_SID), (OLD_SID, OLD_SID)]));

    assert!(shown.contains(&format!("Account: user {OLD_SID:?}")), "{shown}");
    let decide::Outcome::Conflict { artifact_diff, .. } = outcome else {
        panic!("a plain install is a conflict: {outcome:?}");
    };
    assert!(artifact_diff.contains(OLD_SID), "{artifact_diff}");
}

/// The user was deleted and not created again: the name names no account, and the registration's SID is shown.
#[skuld::test]
fn a_blob_name_that_names_no_account_shows_the_registrations_sid() {
    let (spec, snapshot) = alices(OLD_SID);

    let (shown, outcome) = install_over(&spec, &snapshot, knowing(&[(OLD_SID, OLD_SID)]));

    assert!(shown.contains(&format!("Account: user {OLD_SID:?}")), "{shown}");
    assert!(matches!(outcome, decide::Outcome::Conflict { .. }), "{outcome:?}");
}

/// A lookup that fails for another reason than "no such account" (a domain controller outage) is no difference and
/// fails nothing: the read shows the blob's account, whichever of the two accounts it could not look up.
#[skuld::test]
fn a_lookup_that_fails_otherwise_shows_the_blobs_account() {
    let (spec, snapshot) = alices(OLD_SID);
    let alice = |name: &str, _: Option<Written<'_>>| match name {
        "alice" => Err(denied(name)),
        _ => Ok(sid_text(OLD_SID)),
    };
    let stored = |name: &str, _: Option<Written<'_>>| match name {
        "alice" => Ok(sid_text(NEW_SID)),
        _ => Err(denied(name)),
    };

    for lookup in [&alice as &dyn Fn(&str, Option<Written<'_>>) -> Result<Sid>, &stored] {
        let (shown, outcome) = install_over(&spec, &snapshot, lookup);

        assert!(shown.contains(r#"Account: user "alice""#), "{shown}");
        assert!(matches!(outcome, decide::Outcome::UpToDate), "{outcome:?}");
    }
}

/// A built-in account is never looked up: no re-created user is behind it.
#[skuld::test]
fn a_built_in_account_is_shown_as_the_blob_has_it_and_is_not_looked_up() {
    let spec = task_spec_as(User::Name("LocalService".to_owned()), Some("30 3 * * *"));
    let snapshot = TaskSnapshot::new(id(), Ok(blob(&spec)), registered_task(&spec, false));

    let (shown, outcome) = install_over(&spec, &snapshot, |name, _| panic!("looked up `{name}`"));

    assert!(shown.contains("Account: service LocalService"), "{shown}");
    assert!(matches!(outcome, decide::Outcome::UpToDate), "{outcome:?}");
}

/// A registration whose XML names no principal is for no account the blob's name names: the definition says so,
/// and an install of it is a conflict, as `task_write_with` treats it as an account change.
#[skuld::test]
fn a_registration_that_names_no_principal_shows_that_it_names_none() {
    let spec = task_spec_as(User::Name("alice".to_owned()), Some("30 3 * * *"));
    let mut registered = registered_task(&spec, false);
    registered.xml = registered.xml.replace("<UserId>alice</UserId>", "");
    let snapshot = TaskSnapshot::new(id(), Ok(blob(&spec)), registered);

    let (shown, outcome) = install_over(&spec, &snapshot, knowing(&[("alice", NEW_SID)]));

    assert!(shown.contains(r#"Account: user "<none>""#), "{shown}");
    assert!(matches!(outcome, decide::Outcome::Conflict { .. }), "{outcome:?}");
}

/// A principal that names no account (a name nothing maps, a SID text that is no SID) is shown as it stands, marked
/// as naming none, and an install over it is a conflict: the registration is for no account the blob's name names.
#[skuld::test]
fn a_principal_that_names_no_account_is_shown_marked() {
    for principal in ["ghost", "S-1-5-garbage"] {
        let spec = task_spec_as(User::Name("alice".to_owned()), Some("30 3 * * *"));
        let mut registered = registered_task(&spec, false);
        registered.xml = registered
            .xml
            .replace("<UserId>alice</UserId>", &format!("<UserId>{principal}</UserId>"));
        let snapshot = TaskSnapshot::new(id(), Ok(blob(&spec)), registered);

        let (shown, outcome) = install_over(&spec, &snapshot, knowing(&[("alice", NEW_SID)]));

        assert!(
            shown.contains(&format!("Account: user \"{principal} (no such account)\"")),
            "{shown}"
        );
        assert!(matches!(outcome, decide::Outcome::Conflict { .. }), "{outcome:?}");
    }
}

/// A principal whose text is the blob's own name, for a name that names no account (the user was deleted), is not
/// the blob's account: the read shows a difference, so an install is a conflict, as `enable` and `disable` refuse.
#[skuld::test]
fn a_principal_that_is_the_blobs_name_and_names_no_account_is_a_conflict() {
    let (spec, snapshot) = alices("alice");

    let (shown, outcome) = install_over(&spec, &snapshot, knowing(&[]));

    assert!(!shown.contains(r#"Account: user "alice""#), "{shown}");
    assert!(matches!(outcome, decide::Outcome::Conflict { .. }), "{outcome:?}");
}

/// A blob that did not decode has no account to compare: that is the error, not "the blob's account".
#[skuld::test]
fn the_account_of_a_task_whose_blob_will_not_decode_is_not_compared() {
    let (_, mut snapshot) = alices(OLD_SID);
    snapshot.blob = Err(Arc::new(Error::blob("test")));

    let compared = snapshot.other_account(knowing(&[("alice", NEW_SID), (OLD_SID, OLD_SID)]));

    assert!(compared.is_err(), "{compared:?}");
}

/// A lookup that cannot answer, of either account, is no difference: the blob's account stands.
#[skuld::test]
fn a_lookup_that_cannot_answer_leaves_the_account_to_the_blob() {
    let (_, snapshot) = alices(OLD_SID);
    let alice = |name: &str, _: Option<Written<'_>>| match name {
        "alice" => Err(denied(name)),
        _ => Ok(sid_text(OLD_SID)),
    };
    let stored = |name: &str, _: Option<Written<'_>>| match name {
        "alice" => Ok(sid_text(NEW_SID)),
        _ => Err(denied(name)),
    };

    for lookup in [&alice as &dyn Fn(&str, Option<Written<'_>>) -> Result<Sid>, &stored] {
        assert_eq!(snapshot.other_account(lookup).unwrap(), None);
    }
}

/// A `user.id` the install resolves as a SID string is resolved so by the read, whatever its case: the same
/// SID spelled in lower case is the registration's, so the task is up to date.
#[skuld::test]
fn a_user_id_the_install_accepts_is_the_account_the_read_resolves() {
    let written = OLD_SID.to_lowercase();
    let spec = task_spec_as(User::Id(AccountId::Sid(written.clone())), Some("30 3 * * *"));
    let mut registered = registered_task(&spec, false);
    registered.xml = registered.xml.replace(
        &format!("<UserId>{written}</UserId>"),
        &format!("<UserId>{OLD_SID}</UserId>"),
    );
    assert!(
        registered.xml.contains(OLD_SID),
        "the principal is the SID in upper case"
    );
    let snapshot = TaskSnapshot::new(id(), Ok(blob(&spec)), registered);

    let installed = Scm::new().task_installed(&snapshot).unwrap().definition.to_string();

    assert!(installed.contains(&format!("Account: user {written:?}")), "{installed}");
}

#[skuld::test]
fn a_task_without_goetias_data_is_foreign() {
    let fake = Fake::new();
    fake.on_get(Ok(Some(Registered {
        xml: "<Task/>".to_owned(),
        data: String::new(),
        enabled: Some(true),
    })));

    assert!(matches!(
        scm(&fake).task_read(&id()).unwrap(),
        ReadOutcome::Foreign { recovery: None }
    ));
}

/// The marker alone is proof of ownership, so `uninstall` can still remove a task whose blob is broken.
#[skuld::test]
fn a_task_whose_blob_will_not_decode_is_marked_all_the_same() {
    let spec = task_spec(None);
    let fake = Fake::new();
    fake.on_get(Ok(Some(Registered {
        xml: "<Task/>".to_owned(),
        data: crate::blob::metadata_text(&spec).replace("Spec: ", "Spec: !"),
        enabled: Some(true),
    })));

    let ReadOutcome::Marked(snapshot) = scm(&fake).task_read(&id()).unwrap() else {
        panic!("the marker is there");
    };

    assert!(snapshot.blob().is_err());
}

#[skuld::test]
fn a_missing_task_is_absent_and_a_failed_read_is_the_reads_error() {
    let fake = Fake::new();
    fake.on_get(Ok(None))
        .on_get(Err(Error::os("read the task", io::Error::other("denied"))));
    let scm = scm(&fake);

    assert!(matches!(scm.task_read(&id()).unwrap(), ReadOutcome::Absent));
    assert!(scm.task_read(&id()).is_err());
    assert_eq!(
        fake.calls(),
        [
            Call::Get(r"\Goetia\nightly".to_owned()),
            Call::Get(r"\Goetia\nightly".to_owned())
        ]
    );
}

// Status: what runs ===================================================================================================

#[skuld::test]
fn a_listed_instance_with_a_live_process_is_running_with_its_pid() {
    let fake = Fake::new();
    fake.on_instances(Ok(vec![instance(1, 4242)])).on_info(Ok(TaskInfo {
        last_run: None,
        last_result: 0x0004_1301,
        next_run: None,
    }));

    let status = task_status_of(scm(&fake).task_status_with(&snapshot(), alive).unwrap());

    assert_eq!(status.common.state, State::Running);
    assert_eq!(status.common.pid, Some(4242));
    assert_eq!(status.last_result, None);
}

/// Task Scheduler lists an ended run for a few milliseconds: that is an ended run, not a running one,
/// so `run` is not refused for it.
#[skuld::test]
fn a_listed_instance_whose_process_exited_is_idle_and_does_not_refuse_a_run() {
    let fake = Fake::new();
    fake.on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]))
        .on_info(Ok(TaskInfo {
            last_run: None,
            last_result: 3,
            next_run: None,
        }));
    let scm = scm(&fake);

    assert_eq!(scm.task_state_with(&snapshot(), gone).unwrap(), State::Idle);
    let status = task_status_of(scm.task_status_with(&snapshot(), gone).unwrap());
    assert_eq!(status.common.state, State::Idle);
    assert_eq!(status.common.pid, None);
    assert_eq!(status.last_result, Some(Exit::Code(3)));
}

/// A process goetia may not open is neither running nor idle: the denial is the call's error, worded
/// for the verb and naming the process, and no state is guessed. `status` and `list` report it as
/// `unusable` and `run` refuses with it. Only an Administrator prompt can check a task that runs as
/// another account.
#[skuld::test]
fn a_denied_liveness_probe_is_the_error_naming_the_denial_never_running() {
    let fake = Fake::new();
    fake.on_instances(Ok(vec![instance(1, 4242)]))
        .on_instances(Ok(vec![instance(1, 4242)]));
    let denied = |_pid| Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32));
    let scm = scm(&fake);

    let for_run = scm.task_state_with(&snapshot(), denied).unwrap_err();
    let for_status = scm.task_status_with(&snapshot(), denied).unwrap_err();

    for (error, verb) in [(for_status, "status"), (for_run, "run")] {
        assert_eq!(
            error.raw_os_error(),
            Some(ERROR_ACCESS_DENIED as i32),
            "{verb}: the OS error is kept"
        );
        let said = error.report().to_string();
        assert!(
            said.contains(&format!(
                "`{verb}` was denied access to the process of `nightly`'s run (pid 4242)"
            )),
            "{said}"
        );
    }
}

/// Any other failed probe is the error too, without a denial's wording.
#[skuld::test]
fn a_probe_that_fails_otherwise_is_the_error_never_running() {
    let fake = Fake::new();
    fake.on_instances(Ok(vec![instance(1, 4242)]));
    let failed = |_pid| Err(io::Error::from_raw_os_error(1117));

    let error = scm(&fake).task_state_with(&snapshot(), failed).unwrap_err();

    assert_eq!(error.raw_os_error(), Some(1117));
    assert!(!error.report().to_string().contains("denied access"));
}

#[skuld::test]
fn a_failed_listing_is_the_error_never_idle() {
    let fake = Fake::new();
    let listing_failed = || {
        Err(Error::os(
            "list the runs",
            io::Error::other("the Schedule service stopped"),
        ))
    };
    fake.on_instances(listing_failed()).on_instances(listing_failed());
    let scm = scm(&fake);

    let for_run = scm.task_state_with(&snapshot(), alive).unwrap_err();
    let for_status = scm.task_status_with(&snapshot(), alive).unwrap_err();

    for error in [for_run, for_status] {
        assert!(error.report().to_string().contains("the Schedule service stopped"));
    }
}

/// `run` reads the state before it requests a run: a part of the status that can fail must not refuse it.
#[skuld::test]
fn a_failed_info_does_not_fail_task_state() {
    let fake = Fake::new();
    // No `info` reply is stated: asking for it would panic and fail the test.
    fake.on_instances(Ok(Vec::new()));

    assert_eq!(scm(&fake).task_state_with(&snapshot(), alive).unwrap(), State::Idle);
    fake.finish();
    assert_eq!(fake.calls(), [Call::Instances(r"\Goetia\nightly".to_owned())]);
}

#[skuld::test]
fn a_failed_info_fails_the_status() {
    let fake = Fake::new();
    fake.on_instances(Ok(Vec::new()))
        .on_info(Err(Error::os("read the task", io::Error::other("denied"))));

    assert!(scm(&fake).task_status_with(&snapshot(), alive).is_err());
}

/// `list` and `status` both read a task through `live_status`: the same listing is the same pid.
#[skuld::test]
fn list_and_status_agree_on_a_tasks_pid() {
    let fake = Fake::new();
    for _ in 0..2 {
        fake.on_instances(Ok(vec![instance(1, 4242)])).on_info(Ok(never_run()));
    }
    let scm = scm(&fake);

    let for_status = task_status_of(scm.task_status_with(&snapshot(), alive).unwrap());
    let for_list = task_status_of(scm.task_status_with(&snapshot(), alive).unwrap());

    assert_eq!(for_status, for_list);
    assert_eq!(for_list.common.pid, Some(4242));
}

// scan ================================================================================================================

/// What each scanned name came to, by name.
fn named(scan: &Scan<WindowsSnapshot>) -> Vec<(&str, &'static str)> {
    scan.entries
        .iter()
        .map(|(name, entry)| {
            let kind = match entry {
                ScanEntry::Read(ReadOutcome::Marked(WindowsSnapshot::Service(_))) => "service",
                ScanEntry::Read(ReadOutcome::Marked(WindowsSnapshot::Task(_))) => "task",
                ScanEntry::Read(ReadOutcome::Foreign { .. }) => "foreign",
                ScanEntry::Read(ReadOutcome::Absent) => "absent",
                ScanEntry::Misnamed { .. } => "misnamed",
            };
            (name.as_str(), kind)
        })
        .collect()
}

fn foreign_task() -> Registered {
    Registered {
        xml: "<Task/>".to_owned(),
        data: String::new(),
        enabled: Some(true),
    }
}

#[skuld::test]
fn scan_lists_goetia_folder_tasks() {
    let spec = task_spec(None);
    let fake = Fake::new();
    fake.on_names(Ok(vec!["nightly".to_owned(), "other".to_owned()]))
        .on_get(Ok(Some(registered_task(&spec, false))))
        .on_get(Ok(Some(foreign_task())));

    let scan = scan_tasks(&fake, |_| false);

    assert_eq!(named(&scan), [("nightly", "task"), ("other", "foreign")]);
    assert!(scan.reported.is_empty());
    fake.finish();
}

#[skuld::test]
fn scan_reports_an_unfinished_enumeration_beside_what_it_listed() {
    let spec = task_spec(None);
    let fake = Fake::new();
    fake.on_names(Err(Error::os(
        "list the tasks",
        io::Error::other("the Schedule service stopped"),
    )));

    let scan = scan_tasks(&fake, |_| false);
    assert!(scan.entries.is_empty());
    assert!(matches!(
        scan.reported.as_slice(),
        [Installed::Undetermined { name: None, .. }]
    ));

    // A task that could not be read is the one id left undetermined; the others are still listed.
    let fake = Fake::new();
    fake.on_names(Ok(vec!["broken".to_owned(), "nightly".to_owned()]))
        .on_get(Err(Error::os("read the task", io::Error::other("denied"))))
        .on_get(Ok(Some(registered_task(&spec, false))));

    let scan = scan_tasks(&fake, |_| false);

    assert_eq!(named(&scan), [("nightly", "task")]);
    assert!(matches!(
        scan.reported.as_slice(),
        [Installed::Undetermined { name: Some(name), .. }] if name == "broken"
    ));
}

/// A name that is no id is not goetia's, and nothing reads it.
#[skuld::test]
fn scan_does_not_read_a_name_that_is_no_id() {
    let fake = Fake::new();
    fake.on_names(Ok(vec!["not an id!".to_owned()]));

    let scan = scan_tasks(&fake, |_| false);

    assert!(scan.entries.is_empty());
    assert_eq!(fake.calls(), [Call::Names]);
}

fn service_entry(name: &str, entry: ReadOutcome<ScmSnapshot>) -> (String, ScanEntry<ScmSnapshot>) {
    (name.to_owned(), ScanEntry::Read(entry))
}

fn service() -> ScmSnapshot {
    ScmSnapshot::new(
        Id::try_from("nightly").unwrap(),
        Ok(blob_of(Protocol::Plain)),
        (Parameters::default(), None),
    )
}

/// An id both stores hold reads as the service (`which`), so `list` shows it once, as that, and the
/// task is not even read.
#[skuld::test]
fn scan_lists_an_id_held_in_both_stores_once_as_the_service() {
    let fake = Fake::new();
    fake.on_names(Ok(vec!["Nightly".to_owned(), "weekly".to_owned()]))
        .on_get(Ok(Some(registered_task(&task_spec_named("weekly"), false))));
    let services = Scan {
        entries: vec![service_entry("nightly", ReadOutcome::Marked(service()))],
        reported: Vec::new(),
    };

    let scan = combined(services, &fake);

    assert_eq!(named(&scan), [("nightly", "service"), ("weekly", "task")]);
    assert_eq!(
        fake.calls(),
        [Call::Names, Call::Get(r"\Goetia\weekly".to_owned())],
        "a task the SCM's id covers is not read"
    );
}

/// A foreign service is foreign whatever the task store holds at its name (`which`).
#[skuld::test]
fn scan_lists_a_task_at_a_foreign_services_name_as_nothing() {
    let fake = Fake::new();
    fake.on_names(Ok(vec!["nightly".to_owned()]));
    let services = Scan {
        entries: vec![service_entry("nightly", ReadOutcome::Foreign { recovery: None })],
        reported: Vec::new(),
    };

    let scan = combined(services, &fake);

    assert_eq!(named(&scan), [("nightly", "foreign")]);
    fake.finish();
}

/// The SCM's own report comes first, and Task Scheduler's after it.
#[skuld::test]
fn scan_keeps_both_stores_reports() {
    let fake = Fake::new();
    fake.on_names(Err(Error::os("list the tasks", io::Error::other("stopped"))));
    let services = Scan {
        entries: Vec::new(),
        reported: vec![Installed::scan_incomplete(
            "the registry",
            Error::os("open", io::Error::other("denied")),
        )],
    };

    let scan = combined(services, &fake);

    assert_eq!(scan.reported.len(), 2);
}

fn task_spec_named(name: &str) -> DaemonSpec {
    crate::testing::task(name, vec![crate::spec::test_support::program("job")], None)
}
