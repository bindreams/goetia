//! A task's `install` and `uninstall` over real files in temporary directories (the directories are
//! injectable) and a scripted `launchctl`: the script is the order, since the fake fails a call that is
//! not the next one scripted, and a call left unmade at `assert_spent`. Nothing here touches the
//! system's own directories or services.

use std::path::PathBuf;

use super::super::manager_tests::launchd_over;
use super::super::task::task_tests::{ID, Tree, marked, plists_of, service_spec, task_spec};
use super::super::{INSTALL_CALLS, INSTALL_REQUESTS};
use super::*;
use crate::backend::bounded;
use crate::backend::client::fake::{Entry, Fake, exited, reply};
use crate::backend::{Backend as _, InstalledService as _};
use crate::decide::{self, ExternalConfig, Ownership};
use crate::manager::Step;
use crate::spec::{TaskSpec, User};
use crate::testing::task;

// Fixtures ============================================================================================================

const SCHEDULE: &str = "30 3 * * *";
const LOADED: i32 = 0;
const UNLOADED: i32 = 113;
/// A `print` of a task's job with no process.
const IDLE: &str = "\tstate = not running\n";
/// A `print` of a task's job with a run in flight.
const BUSY: &str = "\tstate = running\n\tpid = 4242\n";
/// A `print` of a job whose `bootout` has returned and whose process has not exited.
const TEARING_DOWN: &str = "\tstate = SIGTERMed\n\tpid = 4242\n";

fn job_label() -> String {
    format!("system/{ID}")
}

fn timer_name() -> String {
    timer_label(ID)
}

fn timer_target() -> String {
    format!("system/{}", timer_name())
}

fn print(target: String, code: i32, body: &str) -> Entry {
    reply(&["print", &target], exited(code, body, ""))
}

fn bootout(target: String) -> Entry {
    reply(&["bootout", &target], exited(0, "", ""))
}

fn bootstrap(path: &Path) -> Entry {
    reply(
        &["bootstrap", "system", path.to_str().expect("a UTF-8 path")],
        exited(0, "", ""),
    )
}

/// The file `<stem>.plist` in the enabled or the staging directory of `tree`.
fn at(tree: &Tree, enabled: bool, stem: &str) -> PathBuf {
    tree.dir(enabled).join(format!("{stem}.plist"))
}

fn text_at(tree: &Tree, enabled: bool, stem: &str) -> String {
    std::fs::read_to_string(at(tree, enabled, stem)).expect("the plist is there")
}

fn exists(tree: &Tree, enabled: bool, stem: &str) -> bool {
    at(tree, enabled, stem).exists()
}

/// `spec` installed in the enabled or the staging directory: its job and its timer, if it has one.
fn installed(tree: &Tree, spec: &DaemonSpec, enabled: bool) -> Plists {
    let plists = plists_of(spec);
    tree.put(enabled, ID, &plists.job);
    if let Some(timer) = &plists.timer {
        tree.put(enabled, &timer_name(), timer);
    }
    plists
}

fn command(program: &str) -> DaemonSpec {
    task(ID, vec![program.to_string()], Some(SCHEDULE))
}

fn manager(fake: &Fake) -> Launchd {
    launchd_over(fake.clone())
}

fn reserved(mgr: &Launchd, step: Step) -> Reservation {
    mgr.reserve(&[step], Budget::Unbounded).expect("reserve")
}

/// A `Create` of `spec` over `tree`.
fn create(tree: &Tree, fake: &Fake, spec: &DaemonSpec) -> Result<WriteOutcome> {
    let mgr = manager(fake);
    let desired = mgr.render(spec)?;
    mgr.write_task(
        tree.dirs(),
        spec,
        &desired,
        &Outcome::Create,
        None,
        &mut reserved(&mgr, Step::Install),
    )
}

/// An `Update` of what `tree` holds to `spec`, as the `Update` `decide` would choose.
fn rewrite(tree: &Tree, fake: &Fake, spec: &DaemonSpec) -> Result<WriteOutcome> {
    rewrite_drawing(tree, fake, spec, &mut reserved(&manager(fake), Step::Install))
}

fn rewrite_drawing(tree: &Tree, fake: &Fake, spec: &DaemonSpec, reservation: &mut Reservation) -> Result<WriteOutcome> {
    let mgr = manager(fake);
    let snapshot = marked(tree.read());
    let desired = mgr.render(spec)?;
    mgr.write_task(
        tree.dirs(),
        spec,
        &desired,
        &Outcome::Update {
            spec_diff: String::new(),
        },
        Some(&snapshot),
        reservation,
    )
}

/// What `install` of `spec` decides over `tree`, without `--force`.
fn decided(tree: &Tree, spec: &DaemonSpec) -> Outcome {
    let mgr = manager(&Fake::new([]));
    let snapshot = marked(tree.read());
    let blob = snapshot.blob().expect("the job's metadata decodes").clone();
    let ownership = Ownership::Ours {
        regenerated: mgr.regenerate(&blob.spec, &snapshot).unwrap().to_string(),
        on_disk: mgr.installed(&snapshot).unwrap().definition.to_string(),
        blob,
    };
    decide::decide(
        &ownership,
        &mgr.render(spec).unwrap().to_string(),
        spec,
        crate::version(),
        false,
        &ExternalConfig::Absent,
    )
}

fn refused_as_foreign(outcome: Result<WriteOutcome>) -> String {
    match outcome {
        Ok(WriteOutcome::Refused(Outcome::RefuseForeign { recovery })) => recovery,
        other => panic!("expected a refusal of a stranger's file, got {other:?}"),
    }
}

// Create ==============================================================================================================

#[skuld::test]
fn a_task_installs_both_plists_in_staging_job_first() {
    let tree = Tree::new();
    let spec = task_spec(Some(SCHEDULE));
    let fake = Fake::new([print(timer_target(), UNLOADED, ""), print(job_label(), UNLOADED, "")]);

    let done = create(&tree, &fake, &spec).expect("a create");

    assert!(matches!(done, WriteOutcome::Done));
    fake.assert_spent("a create reads both labels and sends nothing else");
    let plists = plists_of(&spec);
    assert_eq!(text_at(&tree, false, ID), plists.job);
    assert_eq!(text_at(&tree, false, &timer_name()), plists.timer.unwrap());
    assert!(!exists(&tree, true, ID) && !exists(&tree, true, &timer_name()));
    let left: Vec<_> = std::fs::read_dir(tree.dir(false)).unwrap().collect();
    assert_eq!(left.len(), 2, "no temporary file is left behind: {left:?}");
}

/// A task with no schedule has no timer, and the timer's label is still asked about: a loaded one
/// would start its job.
#[skuld::test]
fn a_task_with_no_schedule_installs_its_job_alone() {
    let tree = Tree::new();
    let fake = Fake::new([print(timer_target(), UNLOADED, ""), print(job_label(), UNLOADED, "")]);

    create(&tree, &fake, &task_spec(None)).expect("a create");

    fake.assert_spent("both labels are read");
    assert!(exists(&tree, false, ID));
    assert!(!exists(&tree, false, &timer_name()));
}

/// A job or a timer launchd already holds under the task's labels, with no plist of ours, is booted out
/// first, the timer before the job.
#[skuld::test]
fn a_create_boots_out_a_stale_timer_and_job_first() {
    let tree = Tree::new();
    let fake = Fake::new(stale_labels());

    create(&tree, &fake, &task_spec(Some(SCHEDULE))).expect("a create");

    fake.assert_spent("the stale labels are booted out, and waited for");
    assert!(exists(&tree, false, ID) && exists(&tree, false, &timer_name()));
}

/// A `Create`'s longest path: both labels loaded, each booted out and waited for.
fn stale_labels() -> Vec<Entry> {
    vec![
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, ""),
        bootout(job_label()),
        print(job_label(), UNLOADED, ""),
    ]
}

/// The job is written first and the timer after it, so a failure after the job leaves a job without a
/// timer: ours, and an `Update` that writes the timer, never a `Conflict`.
#[skuld::test]
fn with_a_failure_after_the_job_the_state_is_a_job_without_a_timer() {
    let tree = Tree::new();
    let spec = task_spec(Some(SCHEDULE));
    tree.put(false, &timer_name(), "a stranger's file\n");
    let fake = Fake::new([print(timer_target(), UNLOADED, ""), print(job_label(), UNLOADED, "")]);

    let recovery = refused_as_foreign(create(&tree, &fake, &spec));

    assert!(recovery.contains(&timer_name()), "{recovery}");
    assert_eq!(
        text_at(&tree, false, ID),
        plists_of(&spec).job,
        "the job was written first"
    );
    assert_eq!(text_at(&tree, false, &timer_name()), "a stranger's file\n");

    // The stranger's file goes; what is left is a job and no timer.
    std::fs::remove_file(at(&tree, false, &timer_name())).unwrap();
    assert!(
        matches!(decided(&tree, &spec), Outcome::Update { .. }),
        "a job without its timer is an update, not a conflict"
    );
    let fake = Fake::new([print(job_label(), UNLOADED, ""), print(timer_target(), UNLOADED, "")]);
    rewrite(&tree, &fake, &spec).expect("an update writes the missing timer");
    fake.assert_spent("the update reads both labels");
    assert_eq!(text_at(&tree, false, &timer_name()), plists_of(&spec).timer.unwrap());
    assert!(matches!(decided(&tree, &spec), Outcome::UpToDate));
}

/// A timer taken by another install of goetia's between the read and the write is a change to re-run
/// over, not a stranger's file.
#[skuld::test]
fn a_timer_another_install_wrote_is_a_concurrent_change() {
    let tree = Tree::new();
    let spec = task_spec(Some(SCHEDULE));
    let taken = at(&tree, false, &timer_name());
    let theirs = plists_of(&spec).timer.unwrap();
    let fake = Fake::new([
        print(timer_target(), UNLOADED, "").then(move || std::fs::write(taken, theirs).unwrap()),
        print(job_label(), UNLOADED, ""),
    ]);

    let e = create(&tree, &fake, &spec).expect_err("the timer's name was taken");

    assert!(matches!(e, Error::ConcurrentChange { .. }), "{e:?}");
}

// Update ==============================================================================================================

/// A job from one install and a timer from another are each as their own metadata would regenerate
/// them, so installing over them is an `Update`, and needs no `--force`.
#[skuld::test]
fn job_b_beside_timer_a_updates_without_force() {
    let tree = Tree::new();
    let (a, b, new) = (command("/bin/echo"), command("/bin/false"), command("/bin/cat"));
    tree.put(false, ID, plists_of(&b).job);
    tree.put(false, &timer_name(), plists_of(&a).timer.unwrap());

    assert!(matches!(decided(&tree, &new), Outcome::Update { .. }));
    let fake = Fake::new([print(job_label(), UNLOADED, ""), print(timer_target(), UNLOADED, "")]);
    rewrite(&tree, &fake, &new).expect("an update");

    fake.assert_spent("nothing is loaded");
    let plists = plists_of(&new);
    assert_eq!(text_at(&tree, false, ID), plists.job);
    assert_eq!(text_at(&tree, false, &timer_name()), plists.timer.unwrap());
}

/// Retyping a service whose neighbour at the timer's name is a stranger's: the timer is created first
/// and refuses, so nothing has changed.
#[skuld::test]
fn a_foreign_file_at_the_timer_path_refuses_with_nothing_changed() {
    let tree = Tree::new();
    let service = plists_of(&service_spec());
    tree.put(false, ID, &service.job);
    tree.put(false, &timer_name(), "a stranger's file\n");
    let fake = Fake::new([print(job_label(), UNLOADED, ""), print(timer_target(), UNLOADED, "")]);

    let recovery = refused_as_foreign(rewrite(&tree, &fake, &task_spec(Some(SCHEDULE))));

    assert!(recovery.contains(&timer_name()), "{recovery}");
    fake.assert_spent("nothing but the two reads was sent");
    assert_eq!(text_at(&tree, false, ID), service.job, "the job was not replaced");
    assert_eq!(text_at(&tree, false, &timer_name()), "a stranger's file\n");
}

/// A schedule edit changes only what the timer holds: a run in flight is not asked about, and the job
/// is neither booted out nor reloaded.
#[skuld::test]
fn a_schedule_edit_leaves_a_run_in_flight_alone() {
    let tree = Tree::new();
    installed(&tree, &task(ID, vec!["/bin/true".into()], Some(SCHEDULE)), true);
    let new = task(ID, vec!["/bin/true".into()], Some("45 4 * * *"));
    let fake = Fake::new([
        print(job_label(), LOADED, BUSY),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        bootstrap(&at(&tree, true, &timer_name())),
    ]);

    rewrite(&tree, &fake, &new).expect("a schedule edit over a run in flight");

    fake.assert_spent("only the timer went out and came back");
    let plists = plists_of(&new);
    assert_eq!(
        text_at(&tree, true, ID),
        plists.job,
        "the job's metadata names the new schedule"
    );
    assert_eq!(text_at(&tree, true, &timer_name()), plists.timer.unwrap());
}

/// A change to the job over a run in flight is refused with the task's wording, the timer armed again
/// first, and nothing is replaced.
#[skuld::test]
fn a_command_edit_over_a_running_task_is_loaded_job_of_kind_task_and_rearms_the_timer() {
    let tree = Tree::new();
    let old = installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let fake = Fake::new([
        print(job_label(), LOADED, BUSY),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, BUSY),
        bootstrap(&at(&tree, true, &timer_name())),
    ]);

    let e = rewrite(&tree, &fake, &command("/bin/false")).expect_err("a run is in flight");

    assert!(
        matches!(
            e,
            Error::LoadedJob {
                kind: DaemonKind::Task,
                ..
            }
        ),
        "{e:?}"
    );
    assert!(e.to_string().contains("has a run in flight"), "{e}");
    fake.assert_spent("the timer is armed again after the refusal");
    assert_eq!(text_at(&tree, true, ID), old.job);
    assert_eq!(text_at(&tree, true, &timer_name()), old.timer.unwrap());
}

/// A failure after the timer went out, other than the refusal, says the timer is not loaded and that
/// re-running install loads it.
#[skuld::test]
fn a_failure_after_the_timer_went_out_names_the_timer_and_says_to_rerun() {
    let tree = Tree::new();
    let old = installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, IDLE),
        reply(&["bootout", &job_label()], exited(5, "", "boom")),
    ]);

    let e = rewrite(&tree, &fake, &command("/bin/false")).expect_err("the job's bootout failed");

    let text = e.to_string();
    assert!(matches!(e, Error::Step { .. }), "{e:?}");
    assert!(text.contains(&timer_name()) && text.contains("not loaded"), "{text}");
    assert!(text.contains("re-run install"), "{text}");
    assert!(matches!(e.cause(), Error::Command { .. }), "{e:?}");
    fake.assert_spent("the install stopped at the failed bootout");
    assert_eq!(text_at(&tree, true, ID), old.job);
}

/// An uninstall that fails after the timer went out says so as an uninstall: the remedy is to run it
/// again, and re-running install would be the wrong one.
#[skuld::test]
fn a_failed_uninstall_after_the_timer_went_out_says_to_rerun_uninstall() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        reply(&["bootout", &job_label()], exited(5, "", "boom")),
    ]);

    let e = uninstall(&tree, &fake).expect_err("the job's bootout failed");

    let text = e.to_string();
    assert!(matches!(e, Error::Step { .. }), "{e:?}");
    assert!(
        text.contains(&timer_name()) && text.contains("uninstall failed"),
        "{text}"
    );
    assert!(
        text.contains("re-run uninstall") && !text.contains("re-run install"),
        "{text}"
    );
    assert!(matches!(e.cause(), Error::Command { .. }), "{e:?}");
    assert!(
        exists(&tree, true, ID) && exists(&tree, true, &timer_name()),
        "nothing was removed"
    );
}

/// A timer label with no plist is not a timer of ours that is waiting to be loaded again: a later
/// failure is told as itself.
#[skuld::test]
fn a_stray_timer_label_does_not_make_a_later_failure_say_the_timer_is_unloaded() {
    let tree = Tree::new();
    installed(&tree, &task_spec(None), true);
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, IDLE),
        reply(&["bootout", &job_label()], exited(5, "", "boom")),
    ]);

    let e = rewrite(&tree, &fake, &task(ID, vec!["/bin/false".into()], None)).expect_err("the job's bootout failed");

    assert!(matches!(e, Error::Command { .. }), "{e:?}");
}

/// An update that changes nothing still makes the directories the spec names, as it always did.
#[skuld::test]
fn an_update_with_nothing_to_change_still_makes_the_specs_directories() {
    let tree = Tree::new();
    let cwd = tree.dir(false).join("kept-cwd");
    let user = nix::unistd::User::from_uid(nix::unistd::getuid())
        .unwrap()
        .expect("the current user has an account")
        .name;
    let DaemonSpec::Task(task) = task_spec(Some(SCHEDULE)) else {
        unreachable!("`task_spec` builds a task");
    };
    let mut fields = task.into_fields();
    fields.common.cwd = Some(cwd.clone());
    fields.common.user = User::Name(user);
    let spec = DaemonSpec::from(TaskSpec::new(fields).unwrap());
    let rendered = manager(&Fake::new([])).render(&spec).unwrap();
    tree.put(false, ID, &rendered.job);
    tree.put(false, &timer_name(), rendered.timer.as_deref().unwrap());
    let fake = Fake::new([print(job_label(), UNLOADED, ""), print(timer_target(), UNLOADED, "")]);

    rewrite(&tree, &fake, &spec).expect("an update with nothing to change");

    fake.assert_spent("only the two reads");
    assert!(cwd.is_dir(), "the directory the spec names was made");
}

/// A timer label launchd holds with no plist of ours (removed by hand, or a stranger's) is booted out
/// with the job; a refusal then has nothing to load it from, and says so instead of loading a path that
/// is not there.
#[skuld::test]
fn a_busy_job_beside_a_timer_label_with_no_timer_file_is_refused_without_a_panic() {
    let tree = Tree::new();
    let old = installed(&tree, &task_spec(None), true);
    let fake = Fake::new([
        print(job_label(), LOADED, BUSY),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, BUSY),
    ]);

    let e = rewrite(&tree, &fake, &task(ID, vec!["/bin/false".into()], None)).expect_err("a run is in flight");

    let text = e.to_string();
    assert!(text.contains(&timer_name()) && text.contains("no plist"), "{text}");
    assert!(
        matches!(
            e.cause(),
            Error::LoadedJob {
                kind: DaemonKind::Task,
                ..
            }
        ),
        "{e:?}"
    );
    fake.assert_spent("nothing is loaded again: there is no file to load");
    assert_eq!(text_at(&tree, true, ID), old.job);
}

/// A `print` state goetia does not recognise is refused, and not called a run in flight.
#[skuld::test]
fn a_state_launchd_does_not_show_is_not_reported_as_a_run_in_flight() {
    let tree = Tree::new();
    let old = installed(&tree, &task_spec(Some(SCHEDULE)), false);
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, "\tstate = someday-new-apple-state\n"),
    ]);

    let e = rewrite(&tree, &fake, &command("/bin/false")).expect_err("a state that is not known");

    let report = e.report().to_string();
    assert!(matches!(e, Error::Step { .. }), "{e:?}");
    assert!(!report.contains("run in flight"), "{report}");
    assert!(report.contains("does not recognise"), "{report}");
    assert!(report.contains("untouched"), "{report}");
    fake.assert_spent("nothing is booted out");
    assert_eq!(text_at(&tree, false, ID), old.job);
}

/// A refused update has created none of the new spec's directories.
#[skuld::test]
fn a_refused_update_creates_none_of_the_specs_directories() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), false);
    let cwd = tree.dir(false).join("new-cwd");
    let DaemonSpec::Task(task) = command("/bin/false") else {
        unreachable!("`command` builds a task");
    };
    let mut fields = task.into_fields();
    fields.common.cwd = Some(cwd.clone());
    let spec = DaemonSpec::from(TaskSpec::new(fields).unwrap());
    let fake = Fake::new([
        print(job_label(), LOADED, BUSY),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, BUSY),
    ]);

    rewrite(&tree, &fake, &spec).expect_err("a run is in flight");

    assert!(!cwd.exists(), "the refusal came first");
}

/// A stranger's file at the timer's name is a refusal whatever it is: a binary plist, bytes that are not
/// UTF-8, or not a file at all, as the read table has it.
#[skuld::test]
fn a_strangers_file_of_any_shape_at_the_timer_path_is_refused_as_foreign() {
    type Put = fn(&Tree);
    let shapes: [(&str, Put); 3] = [
        ("a binary plist", |tree| {
            tree.put(false, &timer_name(), b"bplist00\0\0\0")
        }),
        ("bytes that are not UTF-8", |tree| {
            tree.put(false, &timer_name(), [0xffu8, 0xfe, 0x00, 0xc3])
        }),
        ("a directory", |tree| {
            std::fs::create_dir(at(tree, false, &timer_name())).unwrap()
        }),
    ];
    for (what, put) in shapes {
        let tree = Tree::new();
        put(&tree);
        let fake = Fake::new([print(timer_target(), UNLOADED, ""), print(job_label(), UNLOADED, "")]);

        let recovery = refused_as_foreign(create(&tree, &fake, &task_spec(Some(SCHEDULE))));

        assert!(recovery.contains(&timer_name()), "{what}: {recovery}");
    }
}

/// An idle job that launchd holds is booted out (and waited for) before it is replaced; a task that is
/// not armed is not loaded again.
#[skuld::test]
fn a_command_edit_over_an_idle_loaded_job_boots_it_out_first() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), false);
    let new = command("/bin/false");
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, IDLE),
        bootout(job_label()),
        print(job_label(), UNLOADED, ""),
    ]);

    rewrite(&tree, &fake, &new).expect("an edit over an idle job");

    fake.assert_spent("the job is checked, booted out and waited for; nothing is loaded again");
    assert_eq!(text_at(&tree, false, ID), plists_of(&new).job);
}

#[skuld::test]
fn an_armed_task_is_bootstrapped_job_then_timer_after_an_edit() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let new = command("/bin/false");
    let fake = Fake::new(armed_edit(&tree));

    rewrite(&tree, &fake, &new).expect("an edit of an armed task");

    fake.assert_spent("both come back, the job first");
    let plists = plists_of(&new);
    assert_eq!(text_at(&tree, true, ID), plists.job);
    assert_eq!(text_at(&tree, true, &timer_name()), plists.timer.unwrap());
}

/// An update's longest path: both labels loaded, the timer out, the job idle and out, then both loaded
/// again, the job before the timer.
fn armed_edit(tree: &Tree) -> Vec<Entry> {
    vec![
        print(job_label(), LOADED, IDLE),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, IDLE),
        bootout(job_label()),
        print(job_label(), UNLOADED, ""),
        bootstrap(&at(tree, true, ID)),
        bootstrap(&at(tree, true, &timer_name())),
    ]
}

#[skuld::test]
fn a_disarmed_edited_task_is_left_unloaded() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), false);
    let new = command("/bin/false");
    let fake = Fake::new([print(job_label(), UNLOADED, ""), print(timer_target(), UNLOADED, "")]);

    rewrite(&tree, &fake, &new).expect("an edit of a staged task");

    fake.assert_spent("only the two reads: nothing is loaded");
    assert_eq!(text_at(&tree, false, ID), plists_of(&new).job);
}

/// A service made a task leaves boot enrolment: its job is moved to staging before it is replaced, so it
/// does not start at boot as a task nobody armed.
#[skuld::test]
fn retyping_a_service_to_a_task_leaves_no_boot_enrolment() {
    let tree = Tree::new();
    tree.put(true, ID, plists_of(&service_spec()).job);
    let new = task_spec(Some(SCHEDULE));
    let fake = Fake::new([print(job_label(), UNLOADED, ""), print(timer_target(), UNLOADED, "")]);

    rewrite(&tree, &fake, &new).expect("a retype");

    fake.assert_spent("nothing is loaded, so nothing is booted out or loaded");
    let plists = plists_of(&new);
    assert!(!exists(&tree, true, ID), "no plist is left in the enabled directory");
    assert_eq!(text_at(&tree, false, ID), plists.job);
    assert_eq!(text_at(&tree, false, &timer_name()), plists.timer.unwrap());
}

/// A loaded service is refused as it always was, before anything is written.
#[skuld::test]
fn retyping_a_loaded_service_is_loaded_job_of_kind_service() {
    let tree = Tree::new();
    let service = plists_of(&service_spec());
    tree.put(true, ID, &service.job);
    let fake = Fake::new([
        print(job_label(), LOADED, ""),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, ""),
    ]);

    let e = rewrite(&tree, &fake, &task_spec(Some(SCHEDULE))).expect_err("a loaded service");

    assert!(
        matches!(
            e,
            Error::LoadedJob {
                kind: DaemonKind::Service,
                ..
            }
        ),
        "{e:?}"
    );
    fake.assert_spent("the service is read again, and refused");
    assert_eq!(text_at(&tree, true, ID), service.job);
    assert!(!exists(&tree, false, &timer_name()), "the timer was not created");
}

#[skuld::test]
fn retyping_a_task_to_a_service_leaves_no_boot_enrolment() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let new = service_spec();
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, IDLE),
        bootout(job_label()),
        print(job_label(), UNLOADED, ""),
    ]);

    rewrite(&tree, &fake, &new).expect("a retype");

    fake.assert_spent("the task goes out and nothing is loaded again");
    assert!(!exists(&tree, true, ID), "no plist is left in the enabled directory");
    assert!(!exists(&tree, true, &timer_name()) && !exists(&tree, false, &timer_name()));
    assert_eq!(text_at(&tree, false, ID), plists_of(&new).job);
}

/// The timer is removed before the job is replaced: with the replacement made to fail, the timer is
/// already gone, which leaves a job without a timer and not a timer beside a job that says it has no
/// schedule.
#[skuld::test]
fn dropping_the_schedule_removes_the_timer_before_the_job() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let job = at(&tree, true, ID);
    // After the timer is out, the job's path becomes a directory the new job cannot replace.
    let obstruct = move || {
        std::fs::remove_file(&job).unwrap();
        std::fs::create_dir(&job).unwrap();
        std::fs::write(job.join("occupied"), "").unwrap();
    };
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()).then(obstruct),
        print(timer_target(), UNLOADED, ""),
    ]);

    let e = rewrite(&tree, &fake, &task_spec(None)).expect_err("the job cannot be replaced");

    assert!(matches!(e, Error::Os { .. }), "{e:?}");
    assert!(
        !exists(&tree, true, &timer_name()),
        "the timer was removed before the job was touched"
    );
}

/// A plist removed or moved between the read and the write is a change to re-run over, found before
/// anything is sent that changes a job.
#[skuld::test]
fn a_concurrently_changed_plist_is_a_concurrent_change_error() {
    let tree = Tree::new();
    let old = installed(&tree, &task_spec(Some(SCHEDULE)), false);
    let job = at(&tree, false, ID);
    let gone = job.clone();
    let fake = Fake::new([
        print(job_label(), UNLOADED, "").then(move || std::fs::remove_file(gone).unwrap()),
        print(timer_target(), UNLOADED, ""),
    ]);

    let e = rewrite(&tree, &fake, &command("/bin/false")).expect_err("the job was removed");

    assert!(
        matches!(&e, Error::ConcurrentChange { path: Some(path), .. } if *path == job),
        "{e:?}"
    );
    fake.assert_spent("nothing is sent after the change");
    assert_eq!(text_at(&tree, false, &timer_name()), old.timer.unwrap());
}

// Uninstall ===========================================================================================================

fn uninstall(tree: &Tree, fake: &Fake) -> Result<()> {
    let mgr = manager(fake);
    let snapshot = marked(tree.read());
    mgr.uninstall(snapshot, &mut reserved(&mgr, Step::Uninstall))
}

#[skuld::test]
fn uninstall_boots_out_timer_then_job_then_removes_timer_then_job() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let (job, timer) = (at(&tree, true, ID), at(&tree, true, &timer_name()));
    let both_still_there = move || {
        assert!(
            job.exists() && timer.exists(),
            "a file was removed before the job was out"
        )
    };
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        bootout(job_label()).then(both_still_there.clone()),
        print(job_label(), UNLOADED, "").then(both_still_there),
    ]);

    uninstall(&tree, &fake).expect("an uninstall");

    fake.assert_spent("the timer, then the job");
    assert!(!exists(&tree, true, ID) && !exists(&tree, true, &timer_name()));
}

/// The timer's file goes before the job's: with the timer's removal made to fail, the job is still there.
#[skuld::test]
fn uninstall_removes_the_timer_before_the_job() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), false);
    let timer = at(&tree, false, &timer_name());
    let obstruct = move || {
        std::fs::remove_file(&timer).unwrap();
        std::fs::create_dir(&timer).unwrap();
        std::fs::write(timer.join("occupied"), "").unwrap();
    };
    let fake = Fake::new([
        print(job_label(), UNLOADED, ""),
        print(timer_target(), UNLOADED, "").then(obstruct),
    ]);

    let e = uninstall(&tree, &fake).expect_err("the timer cannot be removed");

    assert!(matches!(e, Error::Os { .. }), "{e:?}");
    assert!(exists(&tree, false, ID), "the job outlives the timer");
}

/// A timer alone at the id is goetia's and unusable; `uninstall` is the way out.
#[skuld::test]
fn uninstall_of_a_timer_only_id_removes_it() {
    let tree = Tree::new();
    tree.put(
        false,
        &timer_name(),
        plists_of(&task_spec(Some(SCHEDULE))).timer.unwrap(),
    );
    let fake = Fake::new([print(job_label(), UNLOADED, ""), print(timer_target(), UNLOADED, "")]);

    uninstall(&tree, &fake).expect("an uninstall of a timer alone");

    fake.assert_spent("both labels are read");
    assert!(!exists(&tree, false, &timer_name()));
}

/// The job's `bootout` ends a run in flight and is waited for: the files go only once the process has.
#[skuld::test]
fn uninstall_of_a_running_armed_task_ends_the_run() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let (job, timer) = (at(&tree, true, ID), at(&tree, true, &timer_name()));
    let both_still_there = move || assert!(job.exists() && timer.exists(), "a file was removed under a run");
    let fake = Fake::new([
        print(job_label(), LOADED, BUSY),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        bootout(job_label()).then(both_still_there.clone()),
        print(job_label(), LOADED, TEARING_DOWN).then(both_still_there),
        print(job_label(), UNLOADED, ""),
    ]);

    uninstall(&tree, &fake).expect("an uninstall over a run");

    fake.assert_spent("the wait went on until launchd no longer had the job");
    assert!(!exists(&tree, true, ID) && !exists(&tree, true, &timer_name()));
}

// Reservation =========================================================================================================

/// What a path left of its reservation: its `needs` count, less what it drew.
fn left(reservation: &Reservation) -> (usize, usize) {
    bounded::test_hook::left(reservation)
}

/// A task's verbs draw what `needs` counts at the longest path, and a draw past it trips the
/// reservation's own assertion, so a count one short fails here. The numbers of `needs` for `install`
/// and `uninstall` are the larger of a service's and a task's, so these paths spend them exactly.
#[skuld::test]
fn the_verbs_draw_no_more_than_they_reserved() {
    let files = Role::COUNTED_FILES;

    // An update's longest path.
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let fake = Fake::new(armed_edit(&tree));
    let mgr = manager(&fake);
    let mut held = reserved(&mgr, Step::Install);
    rewrite_drawing(&tree, &fake, &command("/bin/false"), &mut held).expect("an update");
    assert_eq!(
        left(&held),
        (0, 0),
        "an update's longest path spends an install's reservation"
    );

    // A create's longest path, shorter.
    let tree = Tree::new();
    let fake = Fake::new(stale_labels());
    let mgr = manager(&fake);
    let mut held = reserved(&mgr, Step::Install);
    let spec = task_spec(Some(SCHEDULE));
    let desired = mgr.render(&spec).unwrap();
    mgr.write_task(tree.dirs(), &spec, &desired, &Outcome::Create, None, &mut held)
        .expect("a create");
    assert_eq!(
        left(&held),
        (INSTALL_REQUESTS - 2, (INSTALL_CALLS - 6) * files),
        "a create over stale labels"
    );

    // An uninstall's longest path.
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let fake = Fake::new(armed_uninstall());
    let mgr = manager(&fake);
    let snapshot = marked(tree.read());
    let mut held = reserved(&mgr, Step::Uninstall);
    mgr.uninstall(snapshot, &mut held).expect("an uninstall");
    assert_eq!(
        left(&held),
        (0, 0),
        "an uninstall's longest path spends its reservation"
    );

    // One reaper or one file short of the longest path fails at the draw.
    for (what, requests, calls) in [
        ("reaper", INSTALL_REQUESTS - 1, INSTALL_CALLS * files),
        ("temp file", INSTALL_REQUESTS, INSTALL_CALLS * files - 1),
    ] {
        let tree = Tree::new();
        installed(&tree, &task_spec(Some(SCHEDULE)), true);
        let fake = Fake::new(armed_edit(&tree));
        let mut short = Reservation::make(requests, calls).expect("make a reservation");
        let drawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(rewrite_drawing(&tree, &fake, &command("/bin/false"), &mut short));
        }));
        let payload = drawn.expect_err(&format!("an update a {what} short went unnoticed"));
        let message = payload.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(
            message.contains(&format!("a {what} was drawn past the reservation")),
            "{message}"
        );
    }
}

/// An uninstall's longest path: both labels loaded, the timer out, then the job.
fn armed_uninstall() -> Vec<Entry> {
    vec![
        print(job_label(), LOADED, BUSY),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        bootout(job_label()),
        print(job_label(), UNLOADED, ""),
    ]
}
