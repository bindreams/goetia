//! A task's two artifacts: the rule that regenerates them, the order of every write that touches both,
//! `uninstall`, `enable`, `disable` and `status`, driven through fake `systemctl`, `busctl` and journal
//! seams. The tests that write `/etc/systemd/system` join the `systemd_group` fixture; the rest
//! touch nothing.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use super::super::UNIT_DIR;
use super::super::busctl::Busctl;
use super::super::journal::fake::Fake as JournalFake;
use super::super::journal::{DEACTIVATED, Entry as JournalEntry, MAIN_PROCESS_EXITED, STARTING};
use super::super::manager_tests::QuarantineSweep;
use super::super::systemctl::Systemctl;
use super::super::systemctl::systemctl_tests::{booted, gate_then};
use super::*;
use crate::backend::client::fake::{Entry, Fake, RoleKind, exited, reply};
use crate::backend::{Backend, Identity, ReadOutcome, ScanEntry};
use crate::decide::Outcome;
use crate::manager::conformance::fresh_id;
use crate::manager::{Exit, State, Step, flow};
use crate::spec::{Id, ServiceSpec, TaskSpec};
use crate::testing::cleanup::remove_or_warn;
use crate::testing::groups::{Group, systemd_group};
use crate::testing::{base_fields, task};

// Specs and snapshots =================================================================================================

fn units_of(spec: &DaemonSpec) -> Units {
    generate::units(spec, &Identity { user: "0".to_string() }).expect("a spec that renders")
}

/// A task named `id` that runs `command` on `schedule`, if it has one.
pub(in crate::backend::systemd::manager) fn task_spec(id: &str, command: &str, schedule: Option<&str>) -> DaemonSpec {
    task(id, vec![command.to_string()], schedule)
}

fn service_spec(id: &str) -> DaemonSpec {
    ServiceSpec::new(base_fields(id, vec!["/bin/true".to_string()]))
        .map(DaemonSpec::from)
        .expect("a service")
}

fn blob_of(text: &str) -> Blob {
    generate::extract(text)
        .expect("a text that extracts")
        .expect("a text with a marker")
}

fn timer_of(text: &str) -> Timer {
    Timer {
        text: text.to_string(),
        blob: Ok(blob_of(text)),
    }
}

/// What `read` makes of `spec`'s units as `units` writes them: the job, and the timer if it has one.
pub(in crate::backend::systemd::manager) fn fragment(spec: &DaemonSpec) -> Fragment {
    let units = units_of(spec);
    Fragment::new(
        spec.common().id.clone(),
        Ok(blob_of(&units.service)),
        Body {
            service: Some(units.service),
            timer: units.timer.as_deref().map(timer_of),
        },
    )
}

/// [`fragment`] with no timer, as `read` sees a job whose timer is not there.
fn job_alone(spec: &DaemonSpec) -> Fragment {
    let units = units_of(spec);
    Fragment::new(
        spec.common().id.clone(),
        Ok(blob_of(&units.service)),
        Body {
            service: Some(units.service),
            timer: None,
        },
    )
}

/// `job`'s own job beside `timer`'s own timer: what an interrupted update leaves.
fn mixed(job: &DaemonSpec, timer: &DaemonSpec) -> Fragment {
    let timer = units_of(timer).timer.expect("a timer");
    Fragment::new(
        job.common().id.clone(),
        Ok(blob_of(&units_of(job).service)),
        Body {
            service: Some(units_of(job).service),
            timer: Some(timer_of(&timer)),
        },
    )
}

// Regeneration ========================================================================================================

#[skuld::test]
fn a_job_beside_its_own_timer_regenerates_to_what_is_installed() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));

    assert_eq!(
        regenerate(&spec, &fragment(&spec)).expect("regenerate"),
        units_of(&spec)
    );
}

/// Decision 23: an update that was interrupted between its two writes is two unmodified artifacts,
/// so the install that follows is an `Update` and not a `Conflict`.
#[skuld::test]
fn job_b_beside_timer_a_regenerates_each_from_its_own_blob() {
    let a = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let b = task_spec("x", "/bin/b", Some("30 4 * * *"));

    let regenerated = regenerate(&b, &mixed(&b, &a)).expect("regenerate");

    assert_eq!(regenerated.service, units_of(&b).service);
    assert_eq!(regenerated.timer, units_of(&a).timer);
    assert_ne!(
        units_of(&a).timer,
        units_of(&b).timer,
        "the specs must differ to say anything"
    );
}

/// A job with no timer regenerates with none: a missing timer is not drift, install and enable write it.
#[skuld::test]
fn a_job_without_a_timer_regenerates_without_one() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));

    let regenerated = regenerate(&spec, &job_alone(&spec)).expect("regenerate");

    assert_eq!(regenerated.timer, None);
    assert_eq!(regenerated.service, units_of(&spec).service);
}

/// A timer whose metadata does not decode is regenerated from the job's spec, so it differs from what
/// is on disk: ours, but changed by someone, which a `--force` rewrites.
#[skuld::test]
fn a_timer_that_does_not_decode_regenerates_from_the_jobs_spec() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let mut snapshot = fragment(&spec);
    snapshot.body.timer = Some(Timer {
        text: "[X-Goetia]\nMarker=goetia\nSpec=garbage\n".to_string(),
        blob: Err(std::sync::Arc::new(Error::blob("it will not decode"))),
    });

    assert_eq!(
        regenerate(&spec, &snapshot).expect("regenerate").timer,
        units_of(&spec).timer
    );
}

/// The leftover of an interrupted retype: a service whose timer is still there, regenerated as it is.
#[skuld::test]
fn a_service_beside_a_marked_timer_keeps_the_timer_it_regenerates() {
    let service = service_spec("x");
    let old = task_spec("x", "/bin/a", Some("30 3 * * *"));

    let regenerated = regenerate(&service, &mixed(&service, &old)).expect("regenerate");

    assert_eq!(regenerated.timer, units_of(&old).timer);
}

#[skuld::test]
fn a_hand_edited_timer_differs_from_its_regeneration() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let mut snapshot = fragment(&spec);
    let timer = snapshot.body.timer.as_mut().expect("a timer");
    timer.text = timer.text.replace("AccuracySec=1s", "AccuracySec=5min");

    let regenerated = regenerate(&spec, &snapshot).expect("regenerate");

    assert_ne!(
        regenerated.timer.as_deref(),
        Some(snapshot.timer().expect("a timer").text.as_str())
    );
}

/// `spec` with a name the generator refuses, as a spec its units could never have been written for.
fn padded(spec: &DaemonSpec) -> DaemonSpec {
    let DaemonSpec::Task(task) = spec else {
        unreachable!("a task")
    };
    let mut fields = task.clone().into_fields();
    fields.common.name = " padded".to_string();
    TaskSpec::new(fields)
        .map(DaemonSpec::from)
        .expect("a task whose name only the generator refuses")
}

/// A spec the generator refuses is refused here, for the timer's spec as for the job's: a timer is
/// never generated for a spec its job could not be.
#[skuld::test]
fn regeneration_refuses_what_the_generator_refuses() {
    let plain = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let padded = padded(&plain);
    assert!(
        generate::units(&padded, &Identity { user: "0".to_string() }).is_err(),
        "the premise"
    );

    assert!(regenerate(&padded, &fragment(&plain)).is_err(), "the job's spec");

    let mut snapshot = fragment(&plain);
    snapshot.body.timer = Some(timer_of(&generate::timer_for(&padded).expect("a timer")));
    assert!(regenerate(&plain, &snapshot).is_err(), "the timer's own spec");
}

// What an update does before it replaces the job ======================================================================

#[skuld::test]
fn what_an_update_does_before_it_replaces_the_job() {
    let service = service_spec("x");
    let scheduled = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let unscheduled = task_spec("x", "/bin/a", None);
    for (what, old, new, expected) in [
        (
            "a service becomes a task: its boot link must not run the task",
            fragment(&service),
            &scheduled,
            Before {
                disable_job: true,
                drop_timer: false,
            },
        ),
        (
            "a task becomes a service: its timer must not start a service's job",
            fragment(&scheduled),
            &service,
            Before {
                disable_job: false,
                drop_timer: true,
            },
        ),
        (
            "a scheduled task loses its schedule",
            fragment(&scheduled),
            &unscheduled,
            Before {
                disable_job: false,
                drop_timer: true,
            },
        ),
        ("a task keeps its schedule", fragment(&scheduled), &scheduled, nothing()),
        (
            "a task that had no schedule gets one",
            fragment(&unscheduled),
            &scheduled,
            nothing(),
        ),
        ("a service stays a service", fragment(&service), &service, nothing()),
        (
            "a service whose timer is a leftover of a retype",
            mixed(&service, &scheduled),
            &service,
            Before {
                disable_job: false,
                drop_timer: true,
            },
        ),
    ] {
        assert_eq!(before_replacing(&old, &units_of(new), new), expected, "{what}");
    }
}

fn nothing() -> Before {
    Before {
        disable_job: false,
        drop_timer: false,
    }
}

// Fakes ===============================================================================================================

fn reload() -> Entry {
    reply(&["daemon-reload"], exited(0, "", ""))
}

/// `systemctl --no-block`'s announcement of job `n` for `verb`.
pub(in crate::backend::systemd::manager) fn enqueued(verb: &str, n: u32) -> Entry {
    reply(
        &[verb],
        exited(0, "", &format!("Enqueued anchor job {n} x.service/{verb}.\n")),
    )
}

/// A manager over stated `systemctl`, `busctl` and journal.
pub(in crate::backend::systemd::manager) fn manager(systemctl: &Fake, busctl: Fake, journal: JournalFake) -> Systemd {
    Systemd::with_seams(
        Systemctl::new(Box::new(systemctl.clone()), booted),
        Busctl::new(Box::new(busctl), booted),
        Arc::new(journal),
    )
}

/// A manager that must reach neither `busctl` nor the journal.
fn over(systemctl: &Fake) -> Systemd {
    manager(systemctl, Fake::new([]), JournalFake::new(Vec::new()))
}

/// The version gate's two probes, then every call of `calls`.
fn without_gate(fake: &Fake) -> Vec<Vec<String>> {
    fake.calls().into_iter().skip(2).collect()
}

fn call(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

// start, stop, kill, restart ==========================================================================================

/// A task's start asks for the run and returns, under every budget: the request is `--no-block`, run
/// to completion with no deadline, and nothing waits for the run.
#[skuld::test]
fn start_of_a_task_never_waits_under_any_budget() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));
    for budget in [
        Budget::Immediate,
        Budget::Unbounded,
        Budget::Bounded(Duration::from_secs(600)),
    ] {
        let fake = gate_then([enqueued("start", 7)]);
        let mgr = over(&fake);
        let mut held = Backend::reserve(&mgr, &[Step::Start], budget).expect("reserve");

        Backend::start(&mgr, fragment(&spec), budget, &mut held).expect("start");

        assert_eq!(
            without_gate(&fake),
            [call(&["start", "--no-block", "--show-transaction", "--", "x.service"])],
            "{budget:?}"
        );
        let waited = fake.waits().pop().expect("the start");
        assert_eq!(waited.role, RoleKind::ToCompletion, "{budget:?}");
        assert!(!waited.bounded, "{budget:?}");
    }
}

/// A service's start is as it was: it waits as its budget says.
#[skuld::test]
fn start_of_a_service_still_waits_as_its_budget_says() {
    let fake = gate_then([enqueued("start", 7)]);
    let mgr = over(&fake);
    let mut held = Backend::reserve(&mgr, &[Step::Start], Budget::Unbounded).expect("reserve");

    Backend::start(&mgr, fragment(&service_spec("x")), Budget::Unbounded, &mut held).expect("start");

    assert_eq!(
        without_gate(&fake),
        [call(&["start", "--show-transaction", "--", "x.service"])]
    );
}

#[skuld::test]
fn stop_and_kill_address_the_service_unit() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let fake = gate_then([enqueued("stop", 8), reply(&["kill"], exited(0, "", ""))]);
    let mgr = over(&fake);
    let mut held = Backend::reserve(&mgr, &[Step::Stop], Budget::Unbounded).expect("reserve");

    Backend::stop(&mgr, fragment(&spec), Budget::Unbounded, &mut held).expect("stop");
    Backend::kill(&mgr, fragment(&spec), &mut held).expect("kill");

    assert_eq!(
        without_gate(&fake),
        [
            call(&["stop", "--show-transaction", "--", "x.service"]),
            call(&["kill", "--kill-who=main-fail", "--signal=SIGKILL", "--", "x.service"]),
        ]
    );
}

#[skuld::test]
fn restart_of_a_task_is_the_existing_restart() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let fake = gate_then([enqueued("restart", 9)]);
    let mgr = over(&fake);
    let restart = Backend::request_restart(&mgr).expect("systemd has a native restart");

    restart(&mgr, fragment(&spec)).expect("restart");

    assert_eq!(
        without_gate(&fake),
        [call(&[
            "restart",
            "--no-block",
            "--show-transaction",
            "--",
            "x.service"
        ])]
    );
}

// enable and disable ==================================================================================================

#[skuld::test]
fn enable_of_a_task_enables_the_timer_now_and_never_the_service() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let fake = gate_then([reply(&["enable"], exited(0, "", ""))]);
    let mgr = over(&fake);

    mgr.enable_units(&fragment(&spec), &mut Held::default())
        .expect("enable");

    assert_eq!(without_gate(&fake), [call(&["enable", "--now", "--", "x.timer"])]);
}

/// A service as it always was.
#[skuld::test]
fn enable_of_a_service_enables_its_unit() {
    let fake = gate_then([reply(&["enable"], exited(0, "", ""))]);

    over(&fake)
        .enable_units(&fragment(&service_spec("x")), &mut Held::default())
        .expect("enable");

    assert_eq!(without_gate(&fake), [call(&["enable", "--", "x.service"])]);
}

/// A snapshot whose blob does not decode acts on the units present.
#[skuld::test]
fn enable_and_disable_of_an_undecodable_snapshot_act_on_the_units_present() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let undecodable = |with_job: bool| {
        let units = units_of(&spec);
        Fragment::new(
            spec.common().id.clone(),
            Err(std::sync::Arc::new(Error::blob("it will not decode"))),
            Body {
                service: with_job.then_some(units.service),
                timer: units.timer.as_deref().map(timer_of),
            },
        )
    };

    let fake = gate_then([
        reply(&["enable"], exited(0, "", "")),
        reply(&["enable"], exited(0, "", "")),
    ]);
    over(&fake)
        .enable_units(&undecodable(true), &mut Held::default())
        .expect("enable");
    assert_eq!(
        without_gate(&fake),
        [
            call(&["enable", "--", "x.service"]),
            call(&["enable", "--now", "--", "x.timer"])
        ]
    );

    let fake = gate_then([reply(&["disable"], exited(0, "", ""))]);
    over(&fake).disable_units(&undecodable(false)).expect("disable");
    assert_eq!(without_gate(&fake), [call(&["disable", "--now", "--", "x.timer"])]);
}

/// A run in flight is the job's and goes on: nothing here stops, kills or even names the job.
#[skuld::test]
fn disable_leaves_the_run_in_flight() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let fake = gate_then([reply(&["disable"], exited(0, "", ""))]);

    over(&fake).disable_units(&fragment(&spec)).expect("disable");

    assert_eq!(without_gate(&fake), [call(&["disable", "--now", "--", "x.timer"])]);
}

#[skuld::test]
fn disable_of_a_task_with_no_timer_has_nothing_to_do() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let fake = Fake::new([]);

    over(&fake).disable_units(&job_alone(&spec)).expect("disable");

    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

#[skuld::test]
fn disable_of_a_service_disables_its_unit() {
    let fake = gate_then([reply(&["disable"], exited(0, "", ""))]);

    over(&fake)
        .disable_units(&fragment(&service_spec("x")))
        .expect("disable");

    assert_eq!(without_gate(&fake), [call(&["disable", "--", "x.service"])]);
}

/// Everything a test's id can leave in the unit directory: both units, both drop-in directories, the
/// enablement links, and the quarantine files. Registered before the first write.
struct Leftovers {
    id: String,
    _sweep: QuarantineSweep,
}

impl Leftovers {
    fn new(id: &str) -> Leftovers {
        Leftovers {
            id: id.to_string(),
            _sweep: QuarantineSweep::new(id),
        }
    }
}

impl Drop for Leftovers {
    fn drop(&mut self) {
        for kind in UnitKind::ALL {
            remove_or_warn(&unit_path(&self.id, kind), false);
            remove_or_warn(&dropin_dir(&self.id, kind), true);
            remove_or_warn(
                &Path::new(UNIT_DIR)
                    .join(kind.wants_dir())
                    .join(unit_name(&self.id, kind)),
                false,
            );
        }
    }
}

fn seed(id: &str, kind: UnitKind, text: &str) {
    std::fs::write(unit_path(id, kind), text).unwrap_or_else(|e| panic!("seed {id}{}: {e}", kind.suffix()));
}

fn text_of(id: &str, kind: UnitKind) -> Option<String> {
    std::fs::read_to_string(unit_path(id, kind)).ok()
}

/// A path the fake's action can own.
fn owned(id: &str, kind: UnitKind) -> PathBuf {
    unit_path(id, kind)
}

/// What the manager reads at `id` now, which must be marked.
fn marked(mgr: &Systemd, id: &str) -> Fragment {
    match Backend::read(mgr, &Id::try_from(id).expect("an id")) {
        Ok(ReadOutcome::Marked(snapshot)) => snapshot,
        Ok(ReadOutcome::Absent) => panic!("{id} reads as absent"),
        Ok(ReadOutcome::Foreign { recovery }) => panic!("{id} reads as foreign: {recovery:?}"),
        Err(e) => panic!("{id} could not be read: {e}"),
    }
}

fn install(mgr: &Systemd, spec: &DaemonSpec, force: bool) -> Outcome {
    let mut held = Backend::reserve(mgr, &[Step::Install], Budget::Unbounded).expect("reserve");
    flow::install(mgr, spec, force, &mut held).expect("install")
}

fn held(mgr: &Systemd) -> Held {
    Backend::reserve(mgr, &[Step::Install], Budget::Unbounded).expect("reserve")
}

// enable writes the timer =============================================================================================

/// A task installed with no timer, or whose timer was lost, gets it at `enable`: written, reloaded,
/// enabled now.
#[skuld::test]
fn enable_writes_a_missing_timer_and_enables_it(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let snapshot = job_alone(&spec);
    let written = owned(&id, UnitKind::Timer);
    let fake = gate_then([
        reply(&["daemon-reload"], exited(0, "", "")).then(move || {
            assert!(written.exists(), "the timer is written before the reload that loads it");
        }),
        reply(&["enable"], exited(0, "", "")),
    ]);

    over(&fake)
        .enable_units(&snapshot, &mut Held::default())
        .expect("enable");

    assert_eq!(text_of(&id, UnitKind::Timer), units_of(&spec).timer);
    assert_eq!(
        without_gate(&fake),
        [
            call(&["daemon-reload"]),
            call(&["enable", "--now", "--", &format!("{id}.timer")])
        ]
    );
}

/// `enable` has no retry loop: a timer that appeared since `read` is a change goetia did not see, and
/// it is left as it is.
#[skuld::test]
fn enable_reports_a_timer_that_appeared_as_a_concurrent_change(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let snapshot = job_alone(&spec);
    let foreign = "[Timer]\nOnCalendar=daily\n";
    seed(&id, UnitKind::Timer, foreign);
    let fake = Fake::new([]);

    let e = over(&fake)
        .enable_units(&snapshot, &mut Held::default())
        .expect_err("a timer is there");

    match e {
        Error::ConcurrentChange { id: named, path } => {
            assert_eq!(named, id);
            assert_eq!(path, Some(unit_path(&id, UnitKind::Timer)));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        text_of(&id, UnitKind::Timer).as_deref(),
        Some(foreign),
        "it was left alone"
    );
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

// Create ==============================================================================================================

/// The job is written first: a timer that a stranger put at its name in the meantime stops the create
/// after the job, which is then a job without a timer, and the stranger's file is untouched.
#[skuld::test]
fn a_task_installs_both_units_job_first(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let units = units_of(&spec);
    let foreign = "[Timer]\nOnCalendar=daily\n";
    seed(&id, UnitKind::Timer, foreign);
    let fake = Fake::new([]);
    let mgr = over(&fake);

    let raced = mgr.create_units(&id, &units, &mut held(&mgr)).expect("create_units");

    assert!(matches!(raced, WriteOutcome::Raced));
    assert_eq!(
        text_of(&id, UnitKind::Service),
        Some(units.service.clone()),
        "the job was written first"
    );
    assert_eq!(
        text_of(&id, UnitKind::Timer).as_deref(),
        Some(foreign),
        "the stranger's timer is untouched"
    );
    assert!(
        fake.calls().is_empty(),
        "nothing was reloaded after a stop: {:?}",
        fake.calls()
    );
}

/// Both files are there at the one reload, which follows the last write.
#[skuld::test]
fn a_task_install_reloads_once_after_both_writes(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let (job, timer) = (owned(&id, UnitKind::Service), owned(&id, UnitKind::Timer));
    let fake = gate_then([reply(&["daemon-reload"], exited(0, "", "")).then(move || {
        assert!(job.exists() && timer.exists(), "the reload came before a write");
    })]);

    let outcome = install(&over(&fake), &spec, false);

    assert_eq!(outcome, Outcome::Create);
    assert_eq!(without_gate(&fake), [call(&["daemon-reload"])]);
    let units = units_of(&spec);
    assert_eq!(text_of(&id, UnitKind::Service), Some(units.service));
    assert_eq!(text_of(&id, UnitKind::Timer), units.timer);
}

#[skuld::test]
fn a_task_with_no_schedule_installs_no_timer(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", None);
    let fake = gate_then([reload()]);

    assert_eq!(install(&over(&fake), &spec, false), Outcome::Create);

    assert!(text_of(&id, UnitKind::Service).is_some());
    assert_eq!(text_of(&id, UnitKind::Timer), None);
}

// Half-written states =================================================================================================

/// The job was written and the timer was not: ours, regenerated without a timer, and the install that
/// follows is an `Update` that writes it.
#[skuld::test]
fn a_job_without_a_timer_is_ours_and_install_rewrites_the_timer(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let units = units_of(&spec);
    seed(&id, UnitKind::Service, &units.service);
    let fake = gate_then([reload()]);
    let mgr = over(&fake);

    assert!(matches!(flow::preview(&mgr, &spec), Ok(Outcome::Update { .. })));
    let outcome = install(&mgr, &spec, false);

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert_eq!(text_of(&id, UnitKind::Timer), units.timer);
}

/// A timer with no job is ours and unusable: `status` and `install` refuse it, and `uninstall` is the
/// way out.
#[skuld::test]
fn a_timer_without_its_job_is_ours_unusable_and_uninstall_removes_it(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    seed(&id, UnitKind::Timer, units_of(&spec).timer.as_deref().expect("a timer"));
    let fake = gate_then([reply(&["disable"], exited(0, "", "")), reload()]);
    let mgr = over(&fake);

    let snapshot = marked(&mgr, &id);
    let why = snapshot.blob().expect_err("no job, so no usable blob").to_string();
    assert!(why.contains("the timer's job is missing"), "{why}");
    assert!(matches!(flow::preview(&mgr, &spec), Ok(Outcome::RefuseUnusable { .. })));

    mgr.uninstall_units(snapshot, &mut held(&mgr)).expect("uninstall");

    assert_eq!(text_of(&id, UnitKind::Timer), None);
    assert_eq!(
        without_gate(&fake),
        [
            call(&["disable", "--now", "--", &format!("{id}.timer")]),
            call(&["daemon-reload"])
        ]
    );
}

/// Decision 23: the job of one install beside the timer of another is two unmodified artifacts.
#[skuld::test]
fn job_b_beside_timer_a_updates_without_force(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let a = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let b = task_spec(&id, "/bin/b", Some("30 3 * * *"));
    seed(&id, UnitKind::Service, &units_of(&b).service);
    seed(&id, UnitKind::Timer, units_of(&a).timer.as_deref().expect("a timer"));
    let fake = gate_then([reload()]);
    let mgr = over(&fake);

    let outcome = install(&mgr, &b, false);

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert_eq!(text_of(&id, UnitKind::Timer), units_of(&b).timer);
}

#[skuld::test]
fn a_hand_edited_timer_is_a_conflict(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let units = units_of(&spec);
    seed(&id, UnitKind::Service, &units.service);
    seed(
        &id,
        UnitKind::Timer,
        &units
            .timer
            .as_deref()
            .expect("a timer")
            .replace("AccuracySec=1s", "AccuracySec=5min"),
    );
    let mgr = over(&Fake::new([]));

    assert!(matches!(flow::preview(&mgr, &spec), Ok(Outcome::Conflict { .. })));
}

/// A drop-in is drift for the timer as for the job, and a forced install clears it.
#[skuld::test]
fn a_timer_drop_in_is_drift(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let units = units_of(&spec);
    seed(&id, UnitKind::Service, &units.service);
    seed(&id, UnitKind::Timer, units.timer.as_deref().expect("a timer"));
    let dropin = dropin_dir(&id, UnitKind::Timer);
    std::fs::create_dir(&dropin).expect("create the drop-in directory");
    std::fs::write(dropin.join("override.conf"), "[Timer]\nAccuracySec=5min\n").expect("write the drop-in");
    let fake = gate_then([reload()]);
    let mgr = over(&fake);

    let preview = flow::preview(&mgr, &spec);
    assert!(
        matches!(
            &preview,
            Ok(Outcome::Conflict {
                unclearable_recovery: None,
                ..
            })
        ),
        "a drop-in goetia can clear is a conflict `--force` resolves: {:?}",
        preview.map(|_| ())
    );
    let outcome = install(&mgr, &spec, true);

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert!(
        !dropin.exists(),
        "the forced write clears the timer's own drop-in directory"
    );
}

/// The leftover of an interrupted retype to a service: the timer is still there, ours, and the install
/// that follows removes it.
#[skuld::test]
fn a_service_beside_a_marked_timer_updates_without_force(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let service = service_spec(&id);
    let old = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    seed(&id, UnitKind::Service, &units_of(&service).service);
    seed(&id, UnitKind::Timer, units_of(&old).timer.as_deref().expect("a timer"));
    let timer = owned(&id, UnitKind::Timer);
    let fake = gate_then([
        reply(&["disable"], exited(0, "", "")).then(move || assert!(timer.exists(), "disarmed before removal")),
        reload(),
    ]);
    let mgr = over(&fake);

    let outcome = install(&mgr, &service, false);

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert_eq!(text_of(&id, UnitKind::Timer), None);
    assert_eq!(text_of(&id, UnitKind::Service), Some(units_of(&service).service));
    assert_eq!(
        without_gate(&fake),
        [
            call(&["disable", "--now", "--", &format!("{id}.timer")]),
            call(&["daemon-reload"])
        ]
    );
}

// Retyping ============================================================================================================

/// Writes `new` over what `old` left on disk, with the fake changing `kind`'s file at the first
/// request, as a concurrent actor would: what survives shows which step came before the request.
/// Returns what `update_units` answered.
fn retype_raced_at_the_first_request(
    id: &str,
    old: &DaemonSpec,
    new: &DaemonSpec,
    verb: &str,
    kind: UnitKind,
) -> WriteOutcome {
    let old_units = units_of(old);
    seed(id, UnitKind::Service, &old_units.service);
    if let Some(timer) = &old_units.timer {
        seed(id, UnitKind::Timer, timer);
    }
    let path = owned(id, kind);
    let fake = gate_then([reply(&[verb], exited(0, "", "")).then(move || {
        std::fs::write(&path, "[Unit]\nDescription=changed by someone else\n").expect("change the file");
    })]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, id);

    mgr.update_units(new, &units_of(new), &snapshot, &mut held(&mgr))
        .expect("update_units")
}

#[skuld::test]
fn retyping_a_service_to_a_task_leaves_no_boot_enrolment(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let service = service_spec(&id);
    let new = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    seed(&id, UnitKind::Service, &units_of(&service).service);
    let fake = gate_then([reply(&["disable"], exited(0, "", "")), reload()]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    let wrote = mgr
        .update_units(&new, &units_of(&new), &snapshot, &mut held(&mgr))
        .expect("update_units");

    assert!(matches!(wrote, WriteOutcome::Done));
    // The old job is disabled, which removes its `multi-user.target.wants` link while its
    // `[Install]` is still there to read; the task has none, and nothing enables it or its timer.
    assert_eq!(
        without_gate(&fake),
        [
            call(&["disable", "--", &format!("{id}.service")]),
            call(&["daemon-reload"])
        ]
    );
    assert_eq!(text_of(&id, UnitKind::Service), Some(units_of(&new).service));
    assert_eq!(text_of(&id, UnitKind::Timer), units_of(&new).timer);
}

/// The disable comes before the job is replaced: the file changed at the request is the old one, so
/// the replacement finds it changed, and the new job is never written.
#[skuld::test]
fn a_service_is_disabled_before_its_job_is_replaced(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let service = service_spec(&id);
    let new = task_spec(&id, "/bin/a", Some("30 3 * * *"));

    let wrote = retype_raced_at_the_first_request(&id, &service, &new, "disable", UnitKind::Service);

    assert!(matches!(wrote, WriteOutcome::Raced));
    assert_eq!(
        text_of(&id, UnitKind::Service).as_deref(),
        Some("[Unit]\nDescription=changed by someone else\n"),
        "the changed job is as it was left"
    );
    assert_eq!(
        text_of(&id, UnitKind::Timer),
        units_of(&new).timer,
        "the timer was created first, unarmed: nothing reloaded or enabled it"
    );
}

#[skuld::test]
fn retyping_a_task_to_a_service_removes_the_timer_before_the_job(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let old = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let service = service_spec(&id);

    // A change to the job at the disarming of the timer: the timer is gone, the job is not replaced.
    let wrote = retype_raced_at_the_first_request(&id, &old, &service, "disable", UnitKind::Service);

    assert!(matches!(wrote, WriteOutcome::Raced));
    assert_eq!(
        text_of(&id, UnitKind::Timer),
        None,
        "the timer went before the job was touched"
    );
    assert_eq!(
        text_of(&id, UnitKind::Service).as_deref(),
        Some("[Unit]\nDescription=changed by someone else\n"),
        "the job was not replaced"
    );
}

#[skuld::test]
fn retyping_a_task_to_a_service_disarms_the_timer_and_replaces_the_job(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let old = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let service = service_spec(&id);
    let old_units = units_of(&old);
    seed(&id, UnitKind::Service, &old_units.service);
    seed(&id, UnitKind::Timer, old_units.timer.as_deref().expect("a timer"));
    let (job, timer) = (owned(&id, UnitKind::Service), owned(&id, UnitKind::Timer));
    let was = old_units.service.clone();
    let fake = gate_then([
        reply(&["disable"], exited(0, "", "")).then(move || {
            assert!(timer.exists(), "the timer is disarmed while it is still there to read");
            assert_eq!(std::fs::read_to_string(&job).expect("the job"), was);
        }),
        reload(),
    ]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    let wrote = mgr
        .update_units(&service, &units_of(&service), &snapshot, &mut held(&mgr))
        .expect("update_units");

    assert!(matches!(wrote, WriteOutcome::Done));
    assert_eq!(
        without_gate(&fake),
        [
            call(&["disable", "--now", "--", &format!("{id}.timer")]),
            call(&["daemon-reload"])
        ]
    );
    assert_eq!(text_of(&id, UnitKind::Timer), None);
    assert_eq!(text_of(&id, UnitKind::Service), Some(units_of(&service).service));
}

#[skuld::test]
fn a_scheduled_task_made_scheduleless_disarms_and_removes_the_timer_first(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let old = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let new = task_spec(&id, "/bin/a", None);

    let wrote = retype_raced_at_the_first_request(&id, &old, &new, "disable", UnitKind::Service);

    assert!(matches!(wrote, WriteOutcome::Raced));
    assert_eq!(
        text_of(&id, UnitKind::Timer),
        None,
        "the timer went before the job was touched"
    );

    // And uninterrupted: the disarm, one reload, and a job with no timer.
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let old = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let new = task_spec(&id, "/bin/a", None);
    let old_units = units_of(&old);
    seed(&id, UnitKind::Service, &old_units.service);
    seed(&id, UnitKind::Timer, old_units.timer.as_deref().expect("a timer"));
    let fake = gate_then([reply(&["disable"], exited(0, "", "")), reload()]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.update_units(&new, &units_of(&new), &snapshot, &mut held(&mgr))
        .expect("update_units");

    assert_eq!(
        without_gate(&fake),
        [
            call(&["disable", "--now", "--", &format!("{id}.timer")]),
            call(&["daemon-reload"])
        ]
    );
    assert_eq!(text_of(&id, UnitKind::Timer), None);
    assert_eq!(text_of(&id, UnitKind::Service), Some(units_of(&new).service));
}

/// A schedule changed on a task that has one is a rewrite of the timer, and nothing restarts it: a
/// rewritten `OnCalendar` reaches an armed timer on the reload that follows.
#[skuld::test]
fn a_changed_schedule_rewrites_the_timer_and_restarts_nothing(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let old = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let new = task_spec(&id, "/bin/a", Some("30 5 * * *"));
    let old_units = units_of(&old);
    seed(&id, UnitKind::Service, &old_units.service);
    seed(&id, UnitKind::Timer, old_units.timer.as_deref().expect("a timer"));
    let fake = gate_then([reload()]);

    let outcome = install(&over(&fake), &new, false);

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert_eq!(without_gate(&fake), [call(&["daemon-reload"])]);
    assert_eq!(text_of(&id, UnitKind::Timer), units_of(&new).timer);
}

/// A change to the timer between `read` and its replacement is a race, reported as one.
#[skuld::test]
fn a_timer_changed_since_it_was_read_is_a_race(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let old = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let new = task_spec(&id, "/bin/a", Some("30 5 * * *"));
    let old_units = units_of(&old);
    seed(&id, UnitKind::Service, &old_units.service);
    seed(&id, UnitKind::Timer, old_units.timer.as_deref().expect("a timer"));
    let mgr = over(&Fake::new([]));
    let snapshot = marked(&mgr, &id);
    let changed = "[Timer]\nOnCalendar=hourly\n";
    seed(&id, UnitKind::Timer, changed);

    let wrote = mgr
        .update_units(&new, &units_of(&new), &snapshot, &mut held(&mgr))
        .expect("update_units");

    assert!(matches!(wrote, WriteOutcome::Raced));
    assert_eq!(
        text_of(&id, UnitKind::Timer).as_deref(),
        Some(changed),
        "it was put back as it was"
    );
    assert_eq!(
        text_of(&id, UnitKind::Service),
        Some(units_of(&new).service),
        "the job, written first, stays: the next read finds job B beside a timer, which is not ours"
    );
}

// A timer the id has none of ==========================================================================================

/// A drop-in of the timer's own, at the id of a task that has no timer.
fn seed_timer_dropin(id: &str) -> PathBuf {
    let dropin = dropin_dir(id, UnitKind::Timer);
    std::fs::create_dir(&dropin).expect("create the drop-in directory");
    std::fs::write(dropin.join("override.conf"), "[Timer]\nAccuracySec=5min\n").expect("write the drop-in");
    dropin
}

/// A task with no schedule reads its timer's drop-ins as drift, so a forced write must clear them
/// or the next install reports the same conflict: after it the task is up to date.
#[skuld::test]
fn a_scheduleless_task_s_timer_drop_in_is_cleared_by_a_forced_install(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", None);
    seed(&id, UnitKind::Service, &units_of(&spec).service);
    let dropin = seed_timer_dropin(&id);
    let fake = gate_then([reload()]);
    let mgr = over(&fake);

    let preview = flow::preview(&mgr, &spec);
    assert!(
        matches!(
            &preview,
            Ok(Outcome::Conflict {
                unclearable_recovery: None,
                ..
            })
        ),
        "{:?}",
        preview.map(|_| ())
    );
    let outcome = install(&mgr, &spec, true);

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert!(!dropin.exists(), "the forced write clears it");
    let again = flow::preview(&mgr, &spec);
    assert!(matches!(again, Ok(Outcome::UpToDate)), "{:?}", again.map(|_| ()));
}

/// The same for a task with no timer that becomes a service.
#[skuld::test]
fn a_scheduleless_task_made_a_service_clears_its_timer_drop_in(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let old = task_spec(&id, "/bin/a", None);
    let service = service_spec(&id);
    seed(&id, UnitKind::Service, &units_of(&old).service);
    let dropin = seed_timer_dropin(&id);
    let fake = gate_then([reload()]);

    let outcome = install(&over(&fake), &service, true);

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert!(!dropin.exists());
}

/// A service retyped to a task beside a timer that is not goetia's is refused with nothing changed:
/// no request reached the manager (so the service is still enabled), and neither file was touched.
#[skuld::test]
fn a_service_retyped_to_a_task_beside_a_foreign_timer_is_refused_with_nothing_changed(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let service = service_spec(&id);
    let task = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let job = units_of(&service).service;
    let foreign = "[Timer]\nOnCalendar=daily\n";
    seed(&id, UnitKind::Service, &job);
    seed(&id, UnitKind::Timer, foreign);
    let fake = gate_then([]);

    let outcome = install(&over(&fake), &task, false);

    let Outcome::RefuseForeign { recovery } = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(
        recovery.contains(&unit_path(&id, UnitKind::Timer).display().to_string())
            && recovery.contains(&format!("systemctl disable --now {id}.timer")),
        "the recovery must name the timer, which is what is in the way: {recovery}"
    );
    assert_eq!(text_of(&id, UnitKind::Service), Some(job), "the job is unchanged");
    assert_eq!(
        text_of(&id, UnitKind::Timer).as_deref(),
        Some(foreign),
        "the stranger's timer is untouched"
    );
    assert!(
        without_gate(&fake).is_empty(),
        "nothing was disabled or reloaded: {:?}",
        fake.calls()
    );
}

/// A timer that appeared since `read` and is another install's, not a stranger's, is a race: the
/// re-read finds it, and nothing has been changed.
#[skuld::test]
fn a_goetia_timer_that_appeared_since_the_read_is_a_race(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let service = service_spec(&id);
    let task = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let job = units_of(&service).service;
    seed(&id, UnitKind::Service, &job);
    let fake = Fake::new([]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);
    seed(&id, UnitKind::Timer, units_of(&task).timer.as_deref().expect("a timer"));

    let wrote = mgr
        .update_units(&task, &units_of(&task), &snapshot, &mut held(&mgr))
        .expect("update_units");

    assert!(matches!(wrote, WriteOutcome::Raced));
    assert_eq!(text_of(&id, UnitKind::Service), Some(job));
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

/// The state a crash leaves after the timer was created and before the service was disabled: a timer of
/// goetia's (a task's blob) beside the old service. `read` reads it as ours with both texts, nothing
/// of it is drift, and the install that follows is an `Update` that finishes the retype.
#[skuld::test]
fn a_task_timer_beside_the_old_service_is_ours_and_a_rerun_finishes_the_retype(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let service = service_spec(&id);
    let task = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    seed(&id, UnitKind::Service, &units_of(&service).service);
    seed(&id, UnitKind::Timer, units_of(&task).timer.as_deref().expect("a timer"));
    let fake = gate_then([reply(&["disable"], exited(0, "", "")), reload()]);
    let mgr = over(&fake);

    let snapshot = marked(&mgr, &id);
    assert_eq!(snapshot.kind(), Some(DaemonKind::Service));
    assert!(snapshot.timer().is_some(), "both artifacts are read as ours");
    assert!(matches!(flow::preview(&mgr, &task), Ok(Outcome::Update { .. })));

    let outcome = install(&mgr, &task, false);

    assert!(matches!(outcome, Outcome::Update { .. }), "{outcome:?}");
    assert_eq!(
        without_gate(&fake),
        [
            call(&["disable", "--", &format!("{id}.service")]),
            call(&["daemon-reload"])
        ]
    );
    assert_eq!(text_of(&id, UnitKind::Service), Some(units_of(&task).service));
    assert_eq!(text_of(&id, UnitKind::Timer), units_of(&task).timer);
    let again = flow::preview(&mgr, &task);
    assert!(matches!(again, Ok(Outcome::UpToDate)), "{:?}", again.map(|_| ()));
}

// uninstall ===========================================================================================================

/// The timer is disarmed first, so nothing starts the job between its stop and its removal; the stop
/// is unbounded; the files go timer first.
#[skuld::test]
fn uninstalling_a_task_disarms_the_timer_stops_the_job_and_removes_both(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let units = units_of(&spec);
    seed(&id, UnitKind::Service, &units.service);
    seed(&id, UnitKind::Timer, units.timer.as_deref().expect("a timer"));
    for kind in UnitKind::ALL {
        let dir = dropin_dir(&id, kind);
        std::fs::create_dir(&dir).expect("create a drop-in directory");
        std::fs::write(dir.join("override.conf"), "[Unit]\n").expect("write a drop-in");
    }
    let (job, timer) = (owned(&id, UnitKind::Service), owned(&id, UnitKind::Timer));
    let fake = gate_then([
        reply(&["disable"], exited(0, "", "")).then(move || {
            assert!(
                job.exists() && timer.exists(),
                "nothing is removed before the timer is disarmed"
            )
        }),
        enqueued("stop", 3),
        reload(),
    ]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, &mut held(&mgr)).expect("uninstall");

    assert_eq!(
        without_gate(&fake),
        [
            call(&["disable", "--now", "--", &format!("{id}.timer")]),
            call(&["stop", "--show-transaction", "--", &format!("{id}.service")]),
            call(&["daemon-reload"]),
        ]
    );
    for kind in UnitKind::ALL {
        assert!(!unit_path(&id, kind).exists(), "{kind:?}");
        assert!(!dropin_dir(&id, kind).exists(), "{kind:?}'s drop-in directory");
    }
}

/// A plain service keeps its order: stop, disable, remove, reload, and nothing about a timer.
#[skuld::test]
fn uninstalling_a_service_keeps_its_order(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = service_spec(&id);
    seed(&id, UnitKind::Service, &units_of(&spec).service);
    let fake = gate_then([enqueued("stop", 3), reply(&["disable"], exited(0, "", "")), reload()]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, &mut held(&mgr)).expect("uninstall");

    assert_eq!(
        without_gate(&fake),
        [
            call(&["stop", "--show-transaction", "--", &format!("{id}.service")]),
            call(&["disable", "--", &format!("{id}.service")]),
            call(&["daemon-reload"]),
        ]
    );
    assert!(!unit_path(&id, UnitKind::Service).exists());
}

/// A job without a timer: ours, and uninstalled without a timer to disarm.
#[skuld::test]
fn uninstalling_a_job_without_a_timer_asks_nothing_of_the_timer(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    seed(&id, UnitKind::Service, &units_of(&spec).service);
    let fake = gate_then([enqueued("stop", 3), reload()]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, &mut held(&mgr)).expect("uninstall");

    assert_eq!(
        without_gate(&fake),
        [
            call(&["stop", "--show-transaction", "--", &format!("{id}.service")]),
            call(&["daemon-reload"]),
        ]
    );
}

/// The timer changed after `read`: it is not removed, and the job, still there, is as `read` found it.
#[skuld::test]
fn uninstall_reports_a_timer_that_changed_as_a_concurrent_change(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", Some("30 3 * * *"));
    let units = units_of(&spec);
    seed(&id, UnitKind::Service, &units.service);
    seed(&id, UnitKind::Timer, units.timer.as_deref().expect("a timer"));
    let timer = owned(&id, UnitKind::Timer);
    let changed = "[Timer]\nOnCalendar=hourly\n";
    let fake = gate_then([
        reply(&["disable"], exited(0, "", "")).then(move || std::fs::write(&timer, changed).expect("change")),
        enqueued("stop", 3),
    ]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    let e = mgr
        .uninstall_units(snapshot, &mut held(&mgr))
        .expect_err("the timer changed");

    assert!(
        matches!(&e, Error::ConcurrentChange { path: Some(path), .. } if *path == unit_path(&id, UnitKind::Timer)),
        "{e:?}"
    );
    assert_eq!(text_of(&id, UnitKind::Timer).as_deref(), Some(changed));
    assert_eq!(text_of(&id, UnitKind::Service), Some(units.service), "the job was left");
}

// scan ================================================================================================================

#[skuld::test]
fn scan_lists_a_task_once_and_a_timer_only_id_as_unusable(#[fixture(systemd_group)] _group: &Group) {
    let both = fresh_id("systemd");
    let alone = fresh_id("systemd");
    let foreign = fresh_id("systemd");
    let (_a, _b, _c) = (Leftovers::new(&both), Leftovers::new(&alone), Leftovers::new(&foreign));
    let spec = task_spec(&both, "/bin/a", Some("30 3 * * *"));
    let units = units_of(&spec);
    seed(&both, UnitKind::Service, &units.service);
    seed(&both, UnitKind::Timer, units.timer.as_deref().expect("a timer"));
    seed(&alone, UnitKind::Timer, units.timer.as_deref().expect("a timer"));
    seed(&foreign, UnitKind::Timer, "[Timer]\nOnCalendar=daily\n");
    let mgr = over(&Fake::new([]));

    let scan = Backend::scan(&mgr);

    let of = |id: &str| -> Vec<&ScanEntry<Fragment>> {
        scan.entries
            .iter()
            .filter(|(name, _)| name == id)
            .map(|(_, entry)| entry)
            .collect()
    };
    match of(&both).as_slice() {
        [ScanEntry::Read(ReadOutcome::Marked(snapshot))] => {
            assert!(snapshot.blob().is_ok());
            assert!(
                snapshot.timer().is_some(),
                "the task is read with its timer, not as two entries"
            );
        }
        _ => panic!("a task is one marked entry"),
    }
    match of(&alone).as_slice() {
        [ScanEntry::Read(ReadOutcome::Marked(snapshot))] => {
            let why = snapshot.blob().expect_err("unusable").to_string();
            assert!(why.contains("the timer's job is missing"), "{why}");
        }
        _ => panic!("a timer alone is one marked entry whose job is missing"),
    }
    assert!(
        matches!(
            of(&foreign).as_slice(),
            [ScanEntry::Read(ReadOutcome::Foreign { recovery: Some(_) })]
        ),
        "a foreign timer alone is foreign, which `list` skips"
    );
}

// status ==============================================================================================================

fn show(active: &str, pid: u32) -> Entry {
    show_in(active, pid, "")
}

/// The job's `show` with the `InvocationID` of the run it is in.
fn show_in(active: &str, pid: u32, invocation: &str) -> Entry {
    reply(
        &["show"],
        exited(
            0,
            &format!("ActiveState={active}\nMainPID={pid}\nInvocationID={invocation}\nLoadState=loaded\n"),
            "",
        ),
    )
}

fn timer_show(state: &str) -> Entry {
    reply(
        &["show"],
        exited(0, &format!("UnitFileState={state}\nLoadState=loaded\n"), ""),
    )
}

fn bus(micros: u64) -> Fake {
    Fake::new([
        reply(
            &["--system", "call"],
            exited(0, "o \"/org/freedesktop/systemd1/unit/x_2etimer\"\n", ""),
        ),
        reply(&["--system", "get-property"], exited(0, &format!("t {micros}\n"), "")),
    ])
}

fn starting(invocation: &str, micros: u64) -> JournalEntry {
    JournalEntry::of(&[
        ("MESSAGE_ID", STARTING),
        ("INVOCATION_ID", invocation),
        ("__REALTIME_TIMESTAMP", &micros.to_string()),
    ])
}

fn exited_with(invocation: &str, status: &str) -> JournalEntry {
    JournalEntry::of(&[
        ("MESSAGE_ID", MAIN_PROCESS_EXITED),
        ("INVOCATION_ID", invocation),
        ("EXIT_CODE", "exited"),
        ("EXIT_STATUS", status),
    ])
}

/// State and pid from the job, `enabled` and the next run from the timer, the last run and its
/// result from the journal.
#[skuld::test]
fn a_task_status_is_read_from_the_job_the_timer_and_the_journal() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let systemctl = Fake::new([show("active", 42), timer_show("enabled")]);
    let journal = JournalFake::new(vec![exited_with("a", "3"), starting("a", 1_700_000_000_000_000)]);
    let mgr = manager(&systemctl, bus(1_794_626_807_000_000), journal.clone());

    let status = mgr.status_of(&fragment(&spec)).expect("status");

    let Status::Task(status) = status else {
        panic!("a task's status is a task's")
    };
    assert_eq!(status.common.state, State::Running);
    assert_eq!(status.common.pid, Some(42));
    assert!(status.common.enabled);
    assert_eq!(
        status.next_run,
        Some(UNIX_EPOCH + Duration::from_micros(1_794_626_807_000_000))
    );
    assert_eq!(
        status.last_run,
        Some(UNIX_EPOCH + Duration::from_micros(1_700_000_000_000_000))
    );
    assert_eq!(status.last_result, Some(Exit::Code(3)));
    assert_eq!(journal.asked(), [["UNIT=x.service".to_string()]]);
    assert_eq!(
        systemctl.calls(),
        [
            call(&[
                "show",
                "--property",
                "ActiveState,MainPID,InvocationID,LoadState",
                "--",
                "x.service"
            ]),
            call(&["show", "--property", "UnitFileState,LoadState", "--", "x.timer"]),
        ]
    );
}

/// PID 1 journals a run's terminal entry before the unit leaves `activating` or `deactivating`, so a
/// state read ahead of the journal can pair a run in flight with its own result. The unit's `InvocationID`
/// being the newest run in the journal, with its terminal entry there, says the run has ended: `Idle`,
/// and the result is that run's. Any other pairing is left as read.
#[skuld::test]
fn a_running_unit_whose_own_run_has_a_terminal_entry_is_idle() {
    let status_of = |state: Entry, journal: Vec<JournalEntry>| {
        let spec = task_spec("x", "/bin/a", None);
        let mgr = manager(&Fake::new([state]), Fake::new([]), JournalFake::new(journal));
        match mgr.status_of(&job_alone(&spec)).expect("status") {
            Status::Task(status) => status,
            other => panic!("a task's status is a task's: {other:?}"),
        }
    };
    let deactivated =
        |invocation: &str| JournalEntry::of(&[("MESSAGE_ID", DEACTIVATED), ("INVOCATION_ID", invocation)]);

    // The journal is newest first: the terminal entry, then the Starting entry.
    let ended = status_of(
        show_in("deactivating", 42, "x1"),
        vec![deactivated("x1"), starting("x1", 1_000)],
    );
    assert_eq!((ended.common.state, ended.common.pid), (State::Idle, None), "{ended:?}");
    assert_eq!(ended.last_result, Some(Exit::Code(0)));

    let in_flight = status_of(show_in("active", 42, "x1"), vec![starting("x1", 1_000)]);
    assert_eq!(
        (in_flight.common.state, in_flight.common.pid),
        (State::Running, Some(42)),
        "{in_flight:?}"
    );
    assert_eq!(in_flight.last_result, None);

    // The unit is in a newer run than the journal's newest has recorded: that run's result is the last.
    let older = status_of(
        show_in("activating", 42, "x2"),
        vec![exited_with("w", "3"), starting("w", 1_000)],
    );
    assert_eq!(
        (older.common.state, older.common.pid),
        (State::Running, Some(42)),
        "{older:?}"
    );
    assert_eq!(older.last_result, Some(Exit::Code(3)));

    // A unit with no invocation of its own is not matched with a run.
    let none = status_of(show("active", 42), vec![deactivated("x1"), starting("x1", 1_000)]);
    assert_eq!(none.common.state, State::Running, "{none:?}");
}

/// A task with no timer installed is not enabled and has no next run, and asks neither the manager
/// about a timer nor the bus.
#[skuld::test]
fn no_timer_is_not_enabled_and_has_no_next_run() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let systemctl = Fake::new([show("inactive", 0)]);
    let mgr = manager(&systemctl, Fake::new([]), JournalFake::new(Vec::new()));

    let status = mgr.status_of(&job_alone(&spec)).expect("status");

    let Status::Task(status) = status else {
        panic!("a task's status is a task's")
    };
    assert_eq!(status.common.state, State::Idle);
    assert!(!status.common.enabled);
    assert_eq!(status.next_run, None);
    assert_eq!(
        (status.last_run, status.last_result),
        (None, None),
        "an empty journal has no run"
    );
    assert_eq!(systemctl.calls().len(), 1, "{:?}", systemctl.calls());
}

/// A journal that cannot be read is the error: a status that said "never run" would be made up.
#[skuld::test]
fn a_task_status_with_an_unreadable_journal_is_the_error() {
    let spec = task_spec("x", "/bin/a", None);
    let mgr = manager(
        &Fake::new([show("inactive", 0)]),
        Fake::new([]),
        JournalFake::unreadable(),
    );

    let e = mgr
        .status_of(&job_alone(&spec))
        .expect_err("the journal cannot be read");

    let report = e.report().to_string();
    assert!(report.contains("no journal"), "{e}");
    assert!(
        report.contains("run as root or a member of `systemd-journal`"),
        "{report}"
    );
}

/// The state `run` checks before it requests a run is the job's alone: an unreadable journal, which
/// refuses a task's whole status, does not refuse a run.
#[skuld::test]
fn a_tasks_state_is_read_from_the_job_alone() {
    let spec = task_spec("x", "/bin/a", Some("30 3 * * *"));
    let systemctl = Fake::new([show("inactive", 0)]);
    let mgr = manager(&systemctl, Fake::new([]), JournalFake::unreadable());

    let state = Backend::task_state(&mgr, &fragment(&spec)).expect("the job's state");

    assert_eq!(state, State::Idle);
    assert_eq!(
        systemctl.calls(),
        [call(&[
            "show",
            "--property",
            "ActiveState,MainPID,InvocationID,LoadState",
            "--",
            "x.service"
        ])]
    );
}

/// A service's status is the one it always had.
#[skuld::test]
fn a_service_status_is_read_from_its_unit_alone() {
    let systemctl = Fake::new([reply(
        &["show"],
        exited(
            0,
            "ActiveState=active\nMainPID=5\nUnitFileState=enabled\nLoadState=loaded\n",
            "",
        ),
    )]);
    let mgr = over(&systemctl);

    let status = mgr.status_of(&fragment(&service_spec("x"))).expect("status");

    let Status::Service(status) = status else {
        panic!("a service's status is a service's")
    };
    assert_eq!(
        (status.state, status.pid, status.enabled),
        (State::Running, Some(5), true)
    );
}
