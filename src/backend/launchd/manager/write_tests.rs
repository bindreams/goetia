//! A task's `install` and `uninstall` over real files in temporary directories (the directories are
//! injectable) and a scripted `launchctl`: the script is the order, since the fake fails a call that is
//! not the next one scripted, and a call left unmade at `assert_spent`. Nothing here touches the
//! system's own directories or services.

use std::path::PathBuf;

use super::super::State;
use super::super::manager_tests::launchd_over;
use super::super::task::Shown;
use super::super::task::task_tests::{ID, Tree, damaged, marked, plists_of, service_spec, task_spec};
use super::super::{
    INSTALL_CALLS, INSTALL_REQUESTS, TASK_DISABLE_CALLS, TASK_DISABLE_REQUESTS, TASK_ENABLE_CALLS, TASK_ENABLE_REQUESTS,
};
use super::*;
use crate::backend::InstalledService as _;
use crate::backend::Settle as _;
use crate::backend::bounded;
use crate::backend::client::fake::{Entry, Fake, exited, reply};
use crate::decide::{self, ExternalConfig, Ownership};
use crate::manager::Step;
use crate::spec::{TaskSpec, User};
use crate::testing::elevation::{ELEVATED, require_elevated};
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

pub(in crate::backend::launchd::manager) fn bootstrap(path: &Path) -> Entry {
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
        Scope {
            created: &mut CreatedDirs::default(),
            reservation: &mut reserved(&mgr, Step::Install),
        },
    )
}

/// An `Update` of what `tree` holds to `spec`, as the `Update` `decide` would choose.
fn rewrite(tree: &Tree, fake: &Fake, spec: &DaemonSpec) -> Result<WriteOutcome> {
    rewrite_drawing(tree, fake, spec, &mut reserved(&manager(fake), Step::Install))
}

fn rewrite_drawing(tree: &Tree, fake: &Fake, spec: &DaemonSpec, reservation: &mut Reservation) -> Result<WriteOutcome> {
    rewrite_making(tree, fake, spec, reservation, &mut CreatedDirs::default())
}

/// [`rewrite_drawing`], making the directories the spec names in `created`.
fn rewrite_making(
    tree: &Tree,
    fake: &Fake,
    spec: &DaemonSpec,
    reservation: &mut Reservation,
    created: &mut CreatedDirs,
) -> Result<WriteOutcome> {
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
        Scope { created, reservation },
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

/// Whatever is found at the timer's name after the job was written (a stranger's file, a directory, a
/// timer another install of goetia's wrote), with or without a label booted out first, ends the install as
/// a concurrent change: the job this install wrote stays, and a re-run installs over it.
#[skuld::test]
fn a_create_that_finds_the_timers_name_taken_is_a_concurrent_change_and_the_job_stays() {
    let spec = task_spec(Some(SCHEDULE));
    let created_over = |tree: &Tree, reads: Vec<Entry>, what: &str| {
        let fake = Fake::new(reads);
        let e = create(tree, &fake, &spec).expect_err(what);
        assert!(matches!(e, Error::ConcurrentChange { .. }), "{what}: {e:?}");
        fake.assert_spent(what);
        assert_eq!(text_at(tree, false, ID), plists_of(&spec).job, "{what}: the job stays");
    };
    let unloaded = || vec![print(timer_target(), UNLOADED, ""), print(job_label(), UNLOADED, "")];

    let tree = Tree::new();
    tree.put(false, &timer_name(), "a stranger's file\n");
    created_over(&tree, unloaded(), "a stranger's file");
    assert_eq!(text_at(&tree, false, &timer_name()), "a stranger's file\n");

    let tree = Tree::new();
    std::fs::create_dir(at(&tree, false, &timer_name())).unwrap();
    created_over(&tree, unloaded(), "a directory");

    let tree = Tree::new();
    tree.put(false, &timer_name(), plists_of(&spec).timer.unwrap());
    created_over(&tree, unloaded(), "a timer another install wrote");

    let tree = Tree::new();
    tree.put(false, &timer_name(), "a stranger's file\n");
    created_over(
        &tree,
        vec![
            print(timer_target(), LOADED, ""),
            bootout(timer_target()),
            print(timer_target(), UNLOADED, ""),
            print(job_label(), UNLOADED, ""),
        ],
        "a stranger's file after a label was booted out",
    );
}

/// A job whose timer was not written (a failure after the job) is ours, and an `Update` that writes the
/// timer, never a `Conflict`.
#[skuld::test]
fn a_job_without_a_timer_is_an_update_that_writes_the_timer() {
    let tree = Tree::new();
    let spec = task_spec(Some(SCHEDULE));
    tree.put(false, ID, plists_of(&spec).job);
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

/// A timer label launchd holds after its file was removed by hand is a definition goetia did not write:
/// where an install gives the task a schedule it goes out before the new timer's file is created, so
/// that the new timer is loaded in its place and the stale one stops firing.
#[skuld::test]
fn an_install_that_adds_a_schedule_replaces_a_timer_label_whose_file_is_gone() {
    let tree = Tree::new();
    installed(&tree, &task_spec(None), true);
    let new = task_spec(Some(SCHEDULE));
    let timer_in = at(&tree, true, &timer_name());
    let absent = timer_in.clone();
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()).then(move || assert!(!absent.exists(), "the stray goes before the new timer is made")),
        print(timer_target(), UNLOADED, ""),
        bootstrap(&timer_in),
    ]);

    rewrite(&tree, &fake, &new).expect("an update");

    fake.assert_spent("the stray timer went out and the new one was loaded; the job was left alone");
    assert_eq!(text_at(&tree, true, &timer_name()), plists_of(&new).timer.unwrap());
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

/// A failure after both plists were replaced leaves files that match the spec, so another install finds
/// nothing to change and loads nothing: the headline sends the user to `enable`, which loads what is
/// missing, not to a re-run.
#[skuld::test]
fn a_failed_load_after_the_plists_were_replaced_says_to_enable_not_to_rerun_install() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let new = command("/bin/false");
    let mut script = armed_edit(&tree);
    script.truncate(script.len() - 2);
    script.push(reply(
        &["bootstrap", "system", at(&tree, true, ID).to_str().unwrap()],
        exited(5, "", "boom"),
    ));
    let fake = Fake::new(script);

    let e = rewrite(&tree, &fake, &new).expect_err("the job's bootstrap failed");

    assert!(matches!(e, Error::Step { .. }), "{e:?}");
    assert_eq!(
        e.to_string(),
        format!(
            "`{ID}`'s timer (`{}`) was booted out and this install failed before loading it again, so it is \
             not loaded; the plists are already replaced, so install would find nothing to change: run \
             `goetia daemon enable {ID}` to load what is missing",
            timer_name()
        )
    );
    fake.assert_spent("the install stopped at the failed bootstrap");
    let plists = plists_of(&new);
    assert_eq!(text_at(&tree, true, ID), plists.job);
    assert_eq!(text_at(&tree, true, &timer_name()), plists.timer.unwrap());
    assert!(
        matches!(decided(&tree, &new), Outcome::UpToDate),
        "a second install is up to date, which is why the advice is `enable`"
    );
}

/// The same advice only once every file is written: a failure while the timer's replacement is still
/// to come leaves a timer that differs, which an install does load.
#[skuld::test]
fn a_failure_before_the_timer_was_replaced_still_says_to_rerun_install() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    // After the job is replaced, the timer's path becomes a directory the new timer cannot replace.
    let timer = at(&tree, true, &timer_name());
    let obstruct = move || {
        std::fs::remove_file(&timer).unwrap();
        std::fs::create_dir(&timer).unwrap();
        std::fs::write(timer.join("occupied"), "").unwrap();
    };
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, IDLE),
        bootout(job_label()),
        print(job_label(), UNLOADED, "").then(obstruct),
    ]);

    let e = rewrite(&tree, &fake, &command("/bin/false")).expect_err("the timer cannot be replaced");

    let text = e.to_string();
    assert!(text.contains("re-run install"), "{text}");
    assert!(!text.contains("enable"), "{text}");
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
/// error goes out bare, with no note of the stray's bootout and no claim that the timer is unloaded.
#[skuld::test]
fn a_later_error_after_a_stray_timer_label_went_out_is_bare() {
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
    fake.assert_spent("the install stopped at the failed bootout");
}

/// An update that changes nothing still makes the directories the spec names, as it always did.
#[skuld::test(labels = [ELEVATED])]
fn an_update_with_nothing_to_change_still_makes_the_specs_directories() {
    require_elevated();
    let tree = Tree::new();
    // Under `/tmp`, which root owns on every host: directories are made only where root's own are.
    let root = tempfile::Builder::new()
        .prefix("goetia-cwd-")
        .tempdir_in("/tmp")
        .expect("a directory under /tmp");
    let cwd = root.path().join("kept-cwd");
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

    let mut created = CreatedDirs::under(vec![root.path().to_path_buf()]);
    rewrite_making(
        &tree,
        &fake,
        &spec,
        &mut reserved(&manager(&fake), Step::Install),
        &mut created,
    )
    .expect("an update with nothing to change");

    fake.assert_spent("only the two reads");
    assert!(cwd.is_dir(), "the directory the spec names was made");
}

/// `spec` run as `user` in `cwd`.
fn in_cwd(spec: DaemonSpec, cwd: &Path, user: User) -> DaemonSpec {
    let DaemonSpec::Task(task) = spec else {
        unreachable!("`task_spec` builds a task");
    };
    let mut fields = task.into_fields();
    fields.common.cwd = Some(cwd.to_path_buf());
    fields.common.user = user;
    DaemonSpec::from(TaskSpec::new(fields).unwrap())
}

/// Every refusal about the spec's directories comes before the first action: an update whose `cwd` is
/// refused (it does not exist and is under no root) sends launchd nothing, so the timer it would have
/// booted out is still loaded.
#[skuld::test]
fn an_update_whose_directories_are_refused_sends_launchd_nothing() {
    let tree = Tree::new();
    let old = installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let fake = Fake::new([]);
    let spec = in_cwd(
        task_spec(Some(SCHEDULE)),
        Path::new("/goetia-no-such-root/cwd"),
        User::Root,
    );

    let e = rewrite(&tree, &fake, &spec).expect_err("the cwd is made under no root");

    assert!(matches!(e, Error::DirectoryRefused { .. }), "{e:?}");
    fake.assert_spent("a refused update sends nothing");
    assert_eq!(text_at(&tree, true, ID), old.job);
    assert_eq!(text_at(&tree, true, &timer_name()), old.timer.unwrap());
}

/// uid 0 is handed nothing: a directory made for a task that runs as root keeps the group it was made
/// with (a new directory takes its parent's), where `chown(0, 0)` would make it wheel's.
#[skuld::test(labels = [ELEVATED])]
fn the_cwd_of_a_task_that_runs_as_root_keeps_the_group_it_was_made_with() {
    require_elevated();
    let tree = Tree::new();
    let root = tempfile::Builder::new()
        .prefix("goetia-cwd-")
        .tempdir_in("/tmp")
        .expect("a directory under /tmp");
    // A group other than wheel, which the directories made below take from it.
    let group = nix::unistd::Gid::from_raw(1);
    nix::unistd::chown(root.path(), None, Some(group)).expect("give the directory to a group");
    let control = root.path().join("control");
    std::fs::create_dir(&control).unwrap();
    let gid = |path: &Path| std::os::unix::fs::MetadataExt::gid(&std::fs::metadata(path).unwrap());
    assert_eq!(
        gid(&control),
        group.as_raw(),
        "a directory takes its parent's group here"
    );
    let cwd = root.path().join("root-cwd");
    let spec = in_cwd(task_spec(Some(SCHEDULE)), &cwd, User::Root);
    let rendered = manager(&Fake::new([])).render(&spec).unwrap();
    tree.put(false, ID, &rendered.job);
    tree.put(false, &timer_name(), rendered.timer.as_deref().unwrap());
    let fake = Fake::new([print(job_label(), UNLOADED, ""), print(timer_target(), UNLOADED, "")]);
    let mut created = CreatedDirs::under(vec![root.path().to_path_buf()]);

    rewrite_making(
        &tree,
        &fake,
        &spec,
        &mut reserved(&manager(&fake), Step::Install),
        &mut created,
    )
    .expect("an update with nothing to change");

    assert_eq!(gid(&cwd), group.as_raw(), "nothing was handed over");
}

/// The directory a task's account is to own is the account's by the time the write that can load the
/// definition starts: a job that runs then finds its `cwd` in its final state. One a failed install
/// made is taken back while it is still empty.
#[skuld::test(labels = [ELEVATED])]
fn the_cwd_is_the_accounts_before_the_definition_can_run() {
    require_elevated();
    let tree = Tree::new();
    let root = tempfile::Builder::new()
        .prefix("goetia-cwd-")
        .tempdir_in("/tmp")
        .expect("a directory under /tmp");
    let cwd = root.path().join("handed-cwd");
    // `daemon` is uid 1, an account every macOS host has that is not root.
    let DaemonSpec::Task(task) = task_spec(Some(SCHEDULE)) else {
        unreachable!("`task_spec` builds a task");
    };
    let mut fields = task.into_fields();
    fields.common.cwd = Some(cwd.clone());
    fields.common.user = User::Name("daemon".to_string());
    let spec = DaemonSpec::from(TaskSpec::new(fields).unwrap());
    let rendered = manager(&Fake::new([])).render(&spec).unwrap();
    tree.put(false, ID, &rendered.job);
    tree.put(false, &timer_name(), rendered.timer.as_deref().unwrap());
    let fake = Fake::new([print(job_label(), UNLOADED, ""), print(timer_target(), UNLOADED, "")]);
    let mut created = CreatedDirs::under(vec![root.path().to_path_buf()]);

    rewrite_making(
        &tree,
        &fake,
        &spec,
        &mut reserved(&manager(&fake), Step::Install),
        &mut created,
    )
    .expect("an update with nothing to change");
    let owner = |path: &std::path::Path| std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(path).unwrap());
    assert_eq!(owner(&cwd), 1, "the account's when the directories are made");

    created.settle(|| crate::backend::Standing::No);
    drop(created);
    assert!(!cwd.exists(), "taken back with the definition that never stood");
}

/// A timer label launchd holds with no plist of ours (removed by hand, or a stranger's) is booted out
/// first; a refusal for a run in flight then has nothing to load it from, and goes out as is, loading no
/// path that is not there.
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
    fake.assert_spent("nothing is loaded again: there is no file to load");
    assert_eq!(text_at(&tree, true, ID), old.job);
}

/// A `print` that shows `state = running` and no pid is a run in flight, as launchd reports it.
#[skuld::test]
fn a_running_state_with_no_pid_is_a_run_in_flight() {
    let tree = Tree::new();
    let old = installed(&tree, &task_spec(Some(SCHEDULE)), false);
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, "\tstate = running\n"),
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
    fake.assert_spent("nothing is booted out");
    assert_eq!(text_at(&tree, false, ID), old.job);
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
    let shown = Shown::State(Some("someday-new-apple-state".to_string())).describe(State::Unknown);
    assert!(report.contains(&shown), "{report}");
    assert!(report.contains("untouched"), "{report}");
    fake.assert_spent("nothing is booted out");
    assert_eq!(text_at(&tree, false, ID), old.job);
}

/// A start launchd has accepted and not yet forked shows no pid, and is a run in flight as `stop` has it.
#[skuld::test]
fn a_queued_start_is_a_run_in_flight() {
    let tree = Tree::new();
    let old = installed(&tree, &task_spec(Some(SCHEDULE)), false);
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), LOADED, "\tstate = spawn scheduled\n"),
    ]);

    let e = rewrite(&tree, &fake, &command("/bin/false")).expect_err("a start is queued");

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
    fake.assert_spent("nothing is booted out");
    assert_eq!(text_at(&tree, false, ID), old.job);
}

/// A `print` that failed is worded as `stop` words it, as a read that failed and not as a state goetia
/// does not recognise.
#[skuld::test]
fn a_print_that_failed_is_worded_as_a_failed_read() {
    let tree = Tree::new();
    let old = installed(&tree, &task_spec(Some(SCHEDULE)), false);
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), UNLOADED, ""),
        print(job_label(), 5, ""),
    ]);

    let e = rewrite(&tree, &fake, &command("/bin/false")).expect_err("a read that failed");

    let report = e.report().to_string();
    assert!(matches!(e, Error::Step { .. }), "{e:?}");
    assert!(report.contains(&Shown::Failed.describe(State::Unknown)), "{report}");
    assert!(!report.contains("does not know"), "{report}");
    assert!(report.contains("untouched"), "{report}");
    fake.assert_spent("nothing is booted out");
    assert_eq!(text_at(&tree, false, ID), old.job);
}

/// A refused update has created none of the new spec's directories: the `cwd` is one the install could
/// make, so the refusal for a run in flight is what leaves it missing.
#[skuld::test(labels = [ELEVATED])]
fn a_refused_update_creates_none_of_the_specs_directories() {
    require_elevated();
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), false);
    // Under `/tmp`, which root owns on every host: directories are made only where root's own are.
    let root = tempfile::Builder::new()
        .prefix("goetia-cwd-")
        .tempdir_in("/tmp")
        .expect("a directory under /tmp");
    let cwd = root.path().join("new-cwd");
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

    let mut created = CreatedDirs::under(vec![root.path().to_path_buf()]);
    let e = rewrite_making(
        &tree,
        &fake,
        &spec,
        &mut reserved(&manager(&fake), Step::Install),
        &mut created,
    )
    .expect_err("a run is in flight");

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
        tree.put(false, ID, &plists_of(&service_spec()).job);
        put(&tree);
        let fake = Fake::new([print(job_label(), UNLOADED, ""), print(timer_target(), UNLOADED, "")]);

        let recovery = refused_as_foreign(rewrite(&tree, &fake, &task_spec(Some(SCHEDULE))));

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

/// A job whose metadata will not decode, beside a timer of goetia's, is a task as every verb reads it
/// ([`Plist::kind`]): an update is no retype, so the job stays where it is enabled and both are loaded.
#[skuld::test]
fn an_update_over_a_damaged_job_beside_a_timer_is_no_retype() {
    let tree = Tree::new();
    let spec = task_spec(Some(SCHEDULE));
    let plists = plists_of(&spec);
    tree.put(true, ID, damaged(&plists.job));
    tree.put(true, &timer_name(), plists.timer.as_ref().unwrap());
    let fake = Fake::new([
        print(job_label(), UNLOADED, ""),
        print(timer_target(), UNLOADED, ""),
        bootstrap(&at(&tree, true, ID)),
        bootstrap(&at(&tree, true, &timer_name())),
    ]);

    rewrite(&tree, &fake, &spec).expect("an update");

    fake.assert_spent("the job was written back and loaded with its timer");
    assert_eq!(text_at(&tree, true, ID), plists.job);
    assert!(!exists(&tree, false, ID), "the job was not moved to staging");
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

/// A task left with no timer has nothing to arm, so its job leaves the enabled directory: `status` then
/// reports it as not enabled, as systemd does, and `enable` refuses it for the same reason.
#[skuld::test]
fn dropping_the_schedule_of_an_enabled_task_leaves_its_job_staged() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let new = task_spec(None);
    let fake = Fake::new([
        print(job_label(), LOADED, IDLE),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
    ]);

    rewrite(&tree, &fake, &new).expect("dropping the schedule");

    fake.assert_spent("the timer goes out, and nothing is loaded again");
    assert_eq!(text_at(&tree, false, ID), plists_of(&new).job);
    assert!(!exists(&tree, true, ID), "the job left the enabled directory");
    assert!(!exists(&tree, true, &timer_name()) && !exists(&tree, false, &timer_name()));
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

// enable and disable ==================================================================================================

/// The reservation `enable` and `disable` make for themselves, at their counts.
fn enable_reservation() -> Reservation {
    Reservation::make(TASK_ENABLE_REQUESTS, TASK_ENABLE_CALLS * Role::COUNTED_FILES).expect("make a reservation")
}

fn disable_reservation() -> Reservation {
    Reservation::make(TASK_DISABLE_REQUESTS, TASK_DISABLE_CALLS * Role::COUNTED_FILES).expect("make a reservation")
}

fn enable(tree: &Tree, fake: &Fake) -> Result<()> {
    manager(fake).enable_task(tree.dirs(), marked(tree.read()), &mut enable_reservation())
}

fn disable(tree: &Tree, fake: &Fake) -> Result<()> {
    manager(fake).disable_task(tree.dirs(), marked(tree.read()), &mut disable_reservation())
}

/// The two reads that open an enable or a disable: whether launchd holds the job's label, and the timer's.
fn held_reads(job: bool, timer: bool) -> Vec<Entry> {
    let code = |held: bool| if held { LOADED } else { UNLOADED };
    vec![
        print(job_label(), code(job), ""),
        print(timer_target(), code(timer), ""),
    ]
}

/// The targets of every `bootout` the fake was asked for, in order.
fn booted_out(fake: &Fake) -> Vec<String> {
    fake.calls()
        .into_iter()
        .filter(|call| call[0] == "bootout")
        .map(|call| call[1].clone())
        .collect()
}

/// The job goes to the enabled directory and is loaded before the timer moves, so a crash between the
/// two never leaves a timer that fires at a job launchd does not hold.
#[skuld::test]
fn enable_moves_the_job_then_bootstraps_it_then_the_timer() {
    let tree = Tree::new();
    let spec = task_spec(Some(SCHEDULE));
    installed(&tree, &spec, false);
    let (job_in, timer_in) = (at(&tree, true, ID), at(&tree, true, &timer_name()));
    let (job_there, timer_staged) = (job_in.clone(), at(&tree, false, &timer_name()));
    let timer_there = timer_in.clone();
    let fake = Fake::new(held_reads(false, false).into_iter().chain([
        bootstrap(&job_in).then(move || {
            assert!(
                job_there.exists() && timer_staged.exists(),
                "the job is in place when it is loaded, and the timer is not yet"
            );
        }),
        bootstrap(&timer_in).then(move || assert!(timer_there.exists(), "the timer is in place when it is loaded")),
    ]));

    enable(&tree, &fake).expect("an enable");

    fake.assert_spent("both reads, the job's bootstrap, then the timer's");
    let plists = plists_of(&spec);
    assert_eq!(text_at(&tree, true, ID), plists.job);
    assert_eq!(text_at(&tree, true, &timer_name()), plists.timer.unwrap());
    assert!(!exists(&tree, false, ID) && !exists(&tree, false, &timer_name()));
}

/// An enable that moved the job and was stopped before the timer moved: the job is in place and loaded,
/// and what is left is the timer.
#[skuld::test]
fn enable_is_finished_by_running_it_again() {
    let tree = Tree::new();
    let plists = plists_of(&task_spec(Some(SCHEDULE)));
    tree.put(true, ID, &plists.job);
    tree.put(false, &timer_name(), plists.timer.as_ref().unwrap());
    let fake = Fake::new(held_reads(true, true));

    enable(&tree, &fake).expect("an enable, finished");

    fake.assert_spent("both labels are loaded, so nothing is sent but the reads");
    assert_eq!(text_at(&tree, true, ID), plists.job, "the job is untouched");
    assert_eq!(text_at(&tree, true, &timer_name()), plists.timer.unwrap());
    assert!(!exists(&tree, false, &timer_name()));
}

/// An enable that moved the timer and did not load it (both files in the enabled directory, the timer
/// not loaded): `enabled` reads true, and only the timer's load is left. An early return on `enabled`
/// would never finish it.
#[skuld::test]
fn enable_finishes_a_crash_that_moved_the_timer_but_did_not_load_it() {
    let tree = Tree::new();
    let plists = installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let timer_in = at(&tree, true, &timer_name());
    let fake = Fake::new(held_reads(true, false).into_iter().chain([bootstrap(&timer_in)]));

    enable(&tree, &fake).expect("an enable, finished");

    fake.assert_spent("the timer is loaded, and the job left alone");
    assert_eq!(text_at(&tree, true, ID), plists.job);
}

/// A job with no timer (the crash after an install's job) gets its timer where the job goes, rendered
/// from the job's own metadata, and loaded.
#[skuld::test]
fn enable_creates_a_missing_timer_in_launchdaemons() {
    let tree = Tree::new();
    let plists = plists_of(&task_spec(Some(SCHEDULE)));
    tree.put(false, ID, &plists.job);
    let (job_in, timer_in) = (at(&tree, true, ID), at(&tree, true, &timer_name()));
    let fake = Fake::new(
        held_reads(false, false)
            .into_iter()
            .chain([bootstrap(&job_in), bootstrap(&timer_in)]),
    );

    enable(&tree, &fake).expect("an enable");

    fake.assert_spent("both reads and both bootstraps");
    assert_eq!(text_at(&tree, true, &timer_name()), plists.timer.unwrap());
    assert!(
        !exists(&tree, false, &timer_name()),
        "created where the job is, not in staging"
    );
    let left: Vec<_> = std::fs::read_dir(tree.dir(true)).unwrap().collect();
    assert_eq!(left.len(), 2, "no temporary file is left behind: {left:?}");
}

/// The timer's label launchd holds after its file was removed by hand goes out first, before the job is
/// loaded, and the new timer is loaded: `bootstrap` of a held label would load nothing.
#[skuld::test]
fn enable_replaces_a_timer_label_whose_file_is_gone() {
    let tree = Tree::new();
    let plists = plists_of(&task_spec(Some(SCHEDULE)));
    tree.put(false, ID, &plists.job);
    let (job_in, timer_in) = (at(&tree, true, ID), at(&tree, true, &timer_name()));
    let fake = Fake::new(held_reads(false, true).into_iter().chain([
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
        bootstrap(&job_in),
        bootstrap(&timer_in),
    ]));

    enable(&tree, &fake).expect("an enable");

    fake.assert_spent("the stray out and waited for, the job loaded, the new timer loaded");
    assert_eq!(text_at(&tree, true, &timer_name()), plists.timer.unwrap());
}

/// A stranger's file that appears at the timer's name after `read` ends the enable as foreign, with the
/// recovery that names the path, and the timer is not loaded.
#[skuld::test]
fn a_stranger_at_the_new_timers_name_is_foreign() {
    let tree = Tree::new();
    let plists = plists_of(&task_spec(Some(SCHEDULE)));
    tree.put(false, ID, &plists.job);
    let (job_in, theirs) = (at(&tree, true, ID), at(&tree, true, &timer_name()));
    let fake = Fake::new([
        print(job_label(), UNLOADED, ""),
        print(timer_target(), UNLOADED, "").then(move || std::fs::write(theirs, "a stranger's file\n").unwrap()),
        bootstrap(&job_in),
    ]);

    let e = enable(&tree, &fake).expect_err("the timer's name was taken");

    assert!(
        matches!(&e, Error::Foreign { id, recovery } if id == ID && recovery.contains(&timer_name())),
        "{e:?}"
    );
    fake.assert_spent("the job was loaded, and the timer was not");
    assert_eq!(text_at(&tree, true, &timer_name()), "a stranger's file\n");
}

/// A task that is in place and loaded is enabled already: nothing is sent but the reads.
#[skuld::test]
fn enable_of_a_loaded_task_is_ok() {
    let tree = Tree::new();
    let plists = installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let fake = Fake::new(held_reads(true, true));

    enable(&tree, &fake).expect("an enable of an enabled task");

    fake.assert_spent("nothing is sent but the two reads");
    assert_eq!(text_at(&tree, true, ID), plists.job);
}

/// A load that fails says the enable stopped, and to run it again.
#[skuld::test]
fn a_failed_load_says_to_run_enable_again() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), false);
    let job_in = at(&tree, true, ID);
    let refused = reply(
        &["bootstrap", "system", job_in.to_str().unwrap()],
        exited(5, "", "Bootstrap failed: 5: Input/output error\n"),
    );
    let fake = Fake::new(held_reads(false, false).into_iter().chain([refused]));

    let e = enable(&tree, &fake).expect_err("launchd refused the load");

    assert!(
        matches!(&e, Error::Step { what, .. } if what.contains(&format!("goetia daemon enable {ID}"))),
        "{e:?}"
    );
}

/// The timer goes out first, and is waited for, so that no fire starts a run from here on; then it moves,
/// then the job. The job is never booted out.
#[skuld::test]
fn disable_boots_out_the_timer_before_moving_it_and_never_boots_out_the_job() {
    let tree = Tree::new();
    let spec = task_spec(Some(SCHEDULE));
    installed(&tree, &spec, true);
    let (out_there, gone_there) = (at(&tree, true, &timer_name()), at(&tree, true, &timer_name()));
    let fake = Fake::new(
        held_reads(true, true).into_iter().chain([
            bootout(timer_target()).then(move || assert!(out_there.exists(), "the timer goes out where it is")),
            print(timer_target(), UNLOADED, "")
                .then(move || assert!(gone_there.exists(), "and moves after launchd let go")),
        ]),
    );

    disable(&tree, &fake).expect("a disable");

    fake.assert_spent("both reads, the timer's bootout, and the wait for it");
    assert_eq!(booted_out(&fake), [timer_target()], "the job is not booted out");
    let plists = plists_of(&spec);
    assert_eq!(text_at(&tree, false, ID), plists.job);
    assert_eq!(text_at(&tree, false, &timer_name()), plists.timer.unwrap());
    assert!(!exists(&tree, true, ID) && !exists(&tree, true, &timer_name()));
}

/// A disable that moved the timer and was stopped before the job moved: the job moves, and nothing is
/// booted out. An early return on `enabled` (the timer's directory: staging) would never finish it.
#[skuld::test]
fn disable_finishes_a_crash_that_moved_the_timer_but_not_the_job() {
    let tree = Tree::new();
    let plists = plists_of(&task_spec(Some(SCHEDULE)));
    tree.put(true, ID, &plists.job);
    tree.put(false, &timer_name(), plists.timer.as_ref().unwrap());
    let fake = Fake::new(held_reads(true, false));

    disable(&tree, &fake).expect("a disable, finished");

    fake.assert_spent("nothing is sent but the reads");
    assert!(booted_out(&fake).is_empty());
    assert_eq!(text_at(&tree, false, ID), plists.job);
    assert!(!exists(&tree, true, ID));
}

/// A run in flight goes on: the job is neither booted out nor signalled, and its plist moves.
#[skuld::test]
fn disable_with_a_run_in_flight_leaves_it_running() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let fake = Fake::new([
        print(job_label(), LOADED, BUSY),
        print(timer_target(), LOADED, ""),
        bootout(timer_target()),
        print(timer_target(), UNLOADED, ""),
    ]);

    disable(&tree, &fake).expect("a disable over a run");

    fake.assert_spent("the reads, and the timer's bootout and wait");
    assert_eq!(booted_out(&fake), [timer_target()]);
    assert!(fake.calls().iter().all(|call| call[0] != "kill"), "no signal is sent");
    assert!(exists(&tree, false, ID) && !exists(&tree, true, ID));
}

/// A failure after the timer went out says so, and to run disable again.
#[skuld::test]
fn a_disable_that_fails_after_the_timer_went_out_says_to_run_it_again() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let taken = at(&tree, false, ID);
    let fake = Fake::new(held_reads(true, true).into_iter().chain([
        bootout(timer_target()).then(move || std::fs::write(taken, "in the way\n").unwrap()),
        print(timer_target(), UNLOADED, ""),
    ]));

    let e = disable(&tree, &fake).expect_err("the job's name in staging is taken");

    assert!(
        matches!(&e, Error::Step { what, .. } if what.contains("this disable failed") && what.contains("re-run disable")),
        "{e:?}"
    );
    assert!(matches!(e.cause(), Error::AlreadyExists { .. }), "{e:?}");
}

/// The reservation of a task's `enable` and `disable` is made before the first request: one that cannot
/// be made sends nothing, and fails as the verb of its id.
#[skuld::test]
fn a_task_enable_and_disable_that_cannot_reserve_send_nothing() {
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), false);
    let fake = Fake::new([]);
    let mgr = manager(&fake);
    let _short = bounded::test_hook::temp_files(0);

    let enabled = mgr.enable(marked(tree.read()), &mut Reservation::default());
    let disabled = mgr.disable(marked(tree.read()));

    for (what, result) in [("enable", enabled), ("disable", disabled)] {
        let e = result.expect_err(what);
        assert!(
            matches!(&e, Error::Reservation { id: Some(id), .. } if id == ID),
            "{what}: {e:?}"
        );
    }
    assert_eq!(fake.calls(), Vec::<Vec<String>>::new(), "nothing may be sent");
}

/// A service's `enable` and `disable` are the move of its plist, and send nothing to launchd: here its
/// plist is where each verb wants it, so they do nothing at all.
#[skuld::test]
fn a_service_enable_and_disable_are_unchanged() {
    let fake = Fake::new([]);
    let mgr = manager(&fake);
    let service = plists_of(&service_spec());

    let enabled = Tree::new();
    enabled.put(true, ID, &service.job);
    mgr.enable(marked(enabled.read()), &mut Reservation::default())
        .expect("an enable of an enabled service");
    let staged = Tree::new();
    staged.put(false, ID, &service.job);
    mgr.disable(marked(staged.read()))
        .expect("a disable of a staged service");

    assert_eq!(fake.calls(), Vec::<Vec<String>>::new());
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
    mgr.write_task(
        tree.dirs(),
        &spec,
        &desired,
        &Outcome::Create,
        None,
        Scope {
            created: &mut CreatedDirs::default(),
            reservation: &mut held,
        },
    )
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

    // An enable's longest path: a job staged and not loaded, with no timer file and a timer label
    // launchd holds, so the stray label is booted out and waited for, the job is loaded, the timer
    // created and loaded.
    let enabling = |tree: &Tree, reservation: &mut Reservation| {
        let (job_in, timer_in) = (at(tree, true, ID), at(tree, true, &timer_name()));
        let fake = Fake::new(held_reads(false, true).into_iter().chain([
            bootout(timer_target()),
            print(timer_target(), UNLOADED, ""),
            bootstrap(&job_in),
            bootstrap(&timer_in),
        ]));
        manager(&fake).enable_task(tree.dirs(), marked(tree.read()), reservation)
    };
    let tree = Tree::new();
    tree.put(false, ID, plists_of(&task_spec(Some(SCHEDULE))).job);
    let mut held = enable_reservation();
    enabling(&tree, &mut held).expect("an enable");
    assert_eq!(left(&held), (0, 0), "an enable's longest path spends its reservation");

    // A disable's: the timer is loaded, so it is booted out and waited for.
    let disabling = |tree: &Tree, reservation: &mut Reservation| {
        let fake = Fake::new(
            held_reads(true, true)
                .into_iter()
                .chain([bootout(timer_target()), print(timer_target(), UNLOADED, "")]),
        );
        manager(&fake).disable_task(tree.dirs(), marked(tree.read()), reservation)
    };
    let tree = Tree::new();
    installed(&tree, &task_spec(Some(SCHEDULE)), true);
    let mut held = disable_reservation();
    disabling(&tree, &mut held).expect("a disable");
    assert_eq!(left(&held), (0, 0), "a disable's longest path spends its reservation");

    // One reaper or one file short of either fails at the draw.
    for (what, requests, calls) in [
        ("reaper", TASK_ENABLE_REQUESTS - 1, TASK_ENABLE_CALLS * files),
        ("temp file", TASK_ENABLE_REQUESTS, TASK_ENABLE_CALLS * files - 1),
    ] {
        let tree = Tree::new();
        tree.put(false, ID, plists_of(&task_spec(Some(SCHEDULE))).job);
        let mut short = Reservation::make(requests, calls).expect("make a reservation");
        let drawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(enabling(&tree, &mut short))));
        let payload = drawn.expect_err(&format!("an enable a {what} short went unnoticed"));
        let message = payload.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(
            message.contains(&format!("a {what} was drawn past the reservation")),
            "{message}"
        );
    }
    for (what, requests, calls) in [
        ("reaper", TASK_DISABLE_REQUESTS - 1, TASK_DISABLE_CALLS * files),
        ("temp file", TASK_DISABLE_REQUESTS, TASK_DISABLE_CALLS * files - 1),
    ] {
        let tree = Tree::new();
        installed(&tree, &task_spec(Some(SCHEDULE)), true);
        let mut short = Reservation::make(requests, calls).expect("make a reservation");
        let drawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(disabling(&tree, &mut short))));
        let payload = drawn.expect_err(&format!("a disable a {what} short went unnoticed"));
        let message = payload.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(
            message.contains(&format!("a {what} was drawn past the reservation")),
            "{message}"
        );
    }

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
