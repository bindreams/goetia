//! `uninstall`'s choice of leaving a run in flight, on systemd: what each sequence does, the stand-in unit
//! it writes for a task's run, and when that goes. Driven through a fake `systemctl`; the files are real,
//! so the tests join the `systemd_group` fixture.

use super::super::systemctl::systemctl_tests::gate_then;
use super::super::task::task_tests::{
    Leftovers, call, enqueued, held, marked, over, reload, seed, service_spec, show, task_spec, text_of, units_of,
    without_gate,
};
use super::super::{Systemd, unit_path};
use super::*;
use crate::backend::client::fake::{Entry, exited, reply};
use crate::backend::systemd::generate;
use crate::backend::{Backend, ReadOutcome};
use crate::decide::Outcome;
use crate::error::Error;
use crate::manager::conformance::fresh_id;
use crate::manager::{RunInFlight, ServiceManager, flow};
use crate::spec::{DaemonSpec, Id};
use crate::testing::groups::{Group, systemd_group};

// Fixtures ============================================================================================================

const SCHEDULE: Option<&str> = Some("30 3 * * *");

/// `systemctl show` of a job systemd has no unit file for.
fn not_loaded() -> Entry {
    reply(
        &["show"],
        exited(
            0,
            "ActiveState=inactive\nMainPID=0\nInvocationID=\nLoadState=not-found\n",
            "",
        ),
    )
}

fn reset_failed() -> Entry {
    reply(&["reset-failed"], exited(0, "", ""))
}

/// The read of a left run that is not active, and the `reset-failed` that follows it only where the run
/// failed: systemd refuses `reset-failed` of a unit that is not in memory, which one that did not fail is not.
fn read_then_clear(failed: bool) -> Vec<Entry> {
    if failed {
        vec![show("failed", 0), reset_failed()]
    } else {
        vec![show("inactive", 0)]
    }
}

/// `calls` end in the reload, and hold the `reset-failed` of `id` (before it) only if the run `failed`.
fn clears_only_a_failed_run(id: &str, calls: &[Vec<String>], failed: bool) {
    let reset = call(&["reset-failed", "--", &format!("{id}.service")]);
    let reload = call(&["daemon-reload"]);
    assert_eq!(calls.contains(&reset), failed, "{calls:?}");
    if failed {
        let at = |wanted: &Vec<String>| calls.iter().position(|seen| seen == wanted);
        assert!(at(&reset) < at(&reload), "before the reload: {calls:?}");
    }
    assert_eq!(calls.last(), Some(&reload), "{calls:?}");
}

fn disabled() -> Entry {
    reply(&["disable"], exited(0, "", ""))
}

/// What `generate::standin` writes for `spec`'s job.
fn standin_of(spec: &DaemonSpec) -> String {
    generate::standin(spec.common().id.as_str(), &units_of(spec).service)
}

/// A stand-in of `spec`'s job on disk, as an earlier `uninstall --no-stop` left it.
fn seed_standin(spec: &DaemonSpec) {
    let path = standin_path(spec.common().id.as_str());
    std::fs::create_dir_all(path.parent().expect("a directory")).expect("create the runtime unit directory");
    std::fs::write(&path, standin_of(spec)).unwrap_or_else(|e| panic!("seed {}: {e}", path.display()));
}

/// `uninstall` of `id`, which nothing installed, through the manager's own front door.
fn uninstall_absent(mgr: &Systemd, id: &str, in_flight: RunInFlight) -> crate::error::Result<()> {
    ServiceManager::uninstall(mgr, &Id::try_from(id).expect("an id"), in_flight)
}

fn standin_text(id: &str) -> Option<String> {
    std::fs::read_to_string(standin_path(id)).ok()
}

/// A task installed on disk, with its timer if it has a schedule.
fn installed(id: &str, schedule: Option<&str>) -> DaemonSpec {
    let spec = task_spec(id, "/bin/a", schedule);
    let units = units_of(&spec);
    seed(id, UnitKind::Service, &units.service);
    if let Some(timer) = &units.timer {
        seed(id, UnitKind::Timer, timer);
    }
    spec
}

fn paths(id: &str) -> [std::path::PathBuf; 2] {
    UnitKind::ALL.map(|kind| unit_path(id, kind))
}

// A service ===========================================================================================================

/// Every step of today's uninstall but the stop: disable, remove, reload.
#[skuld::test]
fn leaving_a_running_service_does_every_step_but_the_stop(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed(&id, UnitKind::Service, &units_of(&service_spec(&id)).service);
    let fake = gate_then([disabled(), reload()]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("uninstall");

    assert_eq!(
        without_gate(&fake),
        [
            call(&["disable", "--", &format!("{id}.service")]),
            call(&["daemon-reload"])
        ]
    );
    assert!(!unit_path(&id, UnitKind::Service).exists());
    assert_eq!(
        standin_text(&id),
        None,
        "a service's run is past its start phase and needs none"
    );
}

// A task: the stand-in is written before the files go and the manager reloads =========================================

#[skuld::test]
fn leaving_a_running_task_writes_the_standin_before_the_removal_and_the_reload(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = installed(&id, SCHEDULE);
    let (before_removal, at_reload) = (paths(&id), paths(&id));
    let (standin_then, standin_now) = (standin_path(&id), standin_path(&id));
    let expected = standin_of(&spec);
    let fake = gate_then([
        disabled(),
        show("activating", 42).then(move || {
            assert!(
                before_removal.iter().all(|p| p.exists()),
                "nothing is removed before the read"
            );
            assert!(
                !standin_then.exists(),
                "the stand-in is written after the read, not before"
            );
        }),
        reload().then(move || {
            assert!(
                at_reload.iter().all(|p| !p.exists()),
                "the files are gone at the reload"
            );
            assert_eq!(
                std::fs::read_to_string(&standin_now).ok().as_deref(),
                Some(expected.as_str()),
                "the stand-in is there at the reload"
            );
        }),
    ]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("uninstall");

    assert_eq!(
        without_gate(&fake),
        [
            call(&["disable", "--now", "--", &format!("{id}.timer")]),
            call(&[
                "show",
                "--property",
                "ActiveState,MainPID,InvocationID,LoadState",
                "--",
                &format!("{id}.service")
            ]),
            call(&["daemon-reload"]),
        ],
        "no stop, and the run is read once, after the timer is disarmed"
    );
    assert_eq!(
        generate::standin_id(&standin_text(&id).expect("a stand-in")).as_deref(),
        Some(id.as_str())
    );
}

#[skuld::test]
fn leaving_a_running_task_with_no_schedule_writes_the_standin_too(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = installed(&id, None);
    let fake = gate_then([show("active", 42), reload()]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("uninstall");

    assert_eq!(
        without_gate(&fake).len(),
        2,
        "a read and a reload: {:?}",
        without_gate(&fake)
    );
    assert_eq!(standin_text(&id), Some(standin_of(&spec)));
    assert!(paths(&id).iter().all(|p| !p.exists()));
}

/// Nothing running is a plain uninstall: today's sequence without the stop, the files gone, no stand-in.
#[skuld::test]
fn leaving_a_task_that_is_not_running_writes_no_standin(#[fixture(systemd_group)] _group: &Group) {
    for state in [show("inactive", 0), show("failed", 0), not_loaded()] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        installed(&id, SCHEDULE);
        let fake = gate_then([disabled(), state, reload()]);
        let mgr = over(&fake);
        let snapshot = marked(&mgr, &id);

        mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
            .expect("uninstall");

        assert_eq!(without_gate(&fake).len(), 3, "{:?}", without_gate(&fake));
        assert_eq!(without_gate(&fake)[2], call(&["daemon-reload"]));
        assert_eq!(standin_text(&id), None);
        assert!(paths(&id).iter().all(|p| !p.exists()));
    }
}

/// `Stop` stops the job and needs no stand-in: it is today's sequence, and the file is never written.
#[skuld::test]
fn stopping_a_task_writes_no_standin(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    installed(&id, SCHEDULE);
    let fake = gate_then([disabled(), enqueued("stop", 3), reload()]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, RunInFlight::Stop, &mut held(&mgr))
        .expect("uninstall");

    assert_eq!(
        without_gate(&fake),
        [
            call(&["disable", "--now", "--", &format!("{id}.timer")]),
            call(&["stop", "--show-transaction", "--", &format!("{id}.service")]),
            call(&["daemon-reload"]),
        ]
    );
    assert_eq!(standin_text(&id), None);
}

/// A file that is not goetia's at the stand-in's path is not clobbered: the uninstall stops with the
/// files still there, where it can be run again.
#[skuld::test]
fn a_foreign_file_at_the_standins_path_stops_the_uninstall_before_anything_is_removed(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    installed(&id, SCHEDULE);
    let foreign = "[Unit]\nDescription=somebody else's runtime unit\n";
    std::fs::create_dir_all(RUNTIME_UNIT_DIR).expect("create the runtime unit directory");
    std::fs::write(standin_path(&id), foreign).expect("seed a foreign runtime unit");
    let fake = gate_then([disabled(), show("active", 42)]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    let e = mgr
        .uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect_err("the stand-in's path is taken");

    assert!(
        matches!(&e, Error::ConcurrentChange { path: Some(path), .. } if *path == standin_path(&id)),
        "{e:?}"
    );
    assert_eq!(
        standin_text(&id).as_deref(),
        Some(foreign),
        "the foreign file is untouched"
    );
    assert!(paths(&id).iter().all(|p| p.exists()), "nothing of the task was removed");
}

// The stand-in's life =================================================================================================

/// A stand-in whose unit is not active goes with the next uninstall of the id, before its reload, after a
/// `reset-failed` where the left run failed.
#[skuld::test]
fn an_uninstall_removes_a_standin_whose_unit_is_not_active(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    for failed in [false, true] {
        for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
            let mut steps = vec![disabled()];
            if in_flight == RunInFlight::Stop {
                steps.push(enqueued("stop", 3));
            }
            steps.extend(read_then_clear(failed));
            steps.push(reload());
            let spec = installed(&id, SCHEDULE);
            seed_standin(&spec);
            let fake = gate_then(steps);
            let mgr = over(&fake);
            let snapshot = marked(&mgr, &id);

            mgr.uninstall_units(snapshot, in_flight, &mut held(&mgr))
                .expect("uninstall");

            clears_only_a_failed_run(&id, &without_gate(&fake), failed);
            assert_eq!(standin_text(&id), None, "{in_flight:?}, failed: {failed}");
            assert!(paths(&id).iter().all(|p| !p.exists()));
        }
    }
}

/// While its unit is active nothing removes it, and the next `Leave` writes it again for the run that is
/// still going (an `install` over a left run does not remove it, and its unit is the installed one then).
#[skuld::test]
fn an_uninstall_keeps_a_standin_whose_unit_is_active(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = installed(&id, SCHEDULE);
    seed_standin(&spec);
    let fake = gate_then([disabled(), show("active", 42), reload()]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("uninstall");

    assert!(
        !without_gate(&fake)
            .iter()
            .any(|c| c[0] == "reset-failed" || c[0] == "stop")
    );
    assert_eq!(standin_text(&id), Some(standin_of(&spec)));
}

/// An `install` of the id removes a stand-in whose unit is not active, reloads, and clears a failed run.
#[skuld::test]
fn an_install_removes_a_standin_whose_unit_is_not_active(#[fixture(systemd_group)] _group: &Group) {
    for failed in [false, true] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        let spec = task_spec(&id, "/bin/a", SCHEDULE);
        seed_standin(&spec);
        let mut steps = read_then_clear(failed);
        steps.extend([reload(), reload()]);
        let fake = gate_then(steps);
        let mgr = over(&fake);

        let outcome = flow::install(
            &mgr,
            &spec,
            false,
            &mut Backend::reserve(
                &mgr,
                &[crate::manager::Step::Install],
                crate::manager::Budget::Unbounded,
            )
            .expect("reserve"),
        )
        .expect("install");

        assert_eq!(outcome, Outcome::Create);
        let calls = without_gate(&fake);
        assert_eq!(calls.len(), 3 + usize::from(failed), "{calls:?}");
        assert_eq!(
            calls.contains(&call(&["reset-failed", "--", &format!("{id}.service")])),
            failed
        );
        assert_eq!(standin_text(&id), None, "failed: {failed}");
        assert_eq!(
            text_of(&id, UnitKind::Service),
            Some(units_of(&spec).service),
            "and the unit is written"
        );
    }
}

/// An `install` of the id is not refused while the left run lives, and leaves the stand-in: systemd adopts
/// the process under the new definition.
#[skuld::test]
fn an_install_over_a_left_run_leaves_the_standin_and_is_not_refused(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", SCHEDULE);
    seed_standin(&spec);
    let fake = gate_then([show("activating", 42), reload()]);
    let mgr = over(&fake);

    let outcome = flow::install(
        &mgr,
        &spec,
        false,
        &mut Backend::reserve(
            &mgr,
            &[crate::manager::Step::Install],
            crate::manager::Budget::Unbounded,
        )
        .expect("reserve"),
    )
    .expect("install");

    assert_eq!(outcome, Outcome::Create);
    assert!(
        !without_gate(&fake)
            .iter()
            .any(|c| c[0] == "stop" || c[0] == "reset-failed")
    );
    assert_eq!(standin_text(&id), Some(standin_of(&spec)));
}

// The absent id =======================================================================================================

/// With nothing of goetia's at the id, `uninstall` of it asks nothing of systemd, whichever choice it has.
#[skuld::test]
fn uninstalling_an_absent_id_with_no_standin_asks_nothing(#[fixture(systemd_group)] _group: &Group) {
    for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        let fake = gate_then([]);
        let mgr = over(&fake);

        let e = uninstall_absent(&mgr, &id, in_flight).expect_err("absent");

        assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
        assert!(without_gate(&fake).is_empty(), "{in_flight:?}: {:?}", fake.calls());
    }
}

/// `Leave` leaves a stand-in whose unit is active, and the run with it.
#[skuld::test]
fn leaving_an_absent_id_whose_standin_is_active_changes_nothing(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", SCHEDULE);
    seed_standin(&spec);
    let fake = gate_then([show("activating", 42)]);
    let mgr = over(&fake);

    let e = uninstall_absent(&mgr, &id, RunInFlight::Leave).expect_err("absent");

    assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
    assert_eq!(
        without_gate(&fake).len(),
        1,
        "one read and nothing else: {:?}",
        fake.calls()
    );
    assert_eq!(standin_text(&id), Some(standin_of(&spec)));
}

#[skuld::test]
fn leaving_an_absent_id_whose_standin_is_not_active_removes_it(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed_standin(&task_spec(&id, "/bin/a", SCHEDULE));
    let fake = gate_then([show("failed", 0), reset_failed(), reload()]);
    let mgr = over(&fake);

    let e = uninstall_absent(&mgr, &id, RunInFlight::Leave).expect_err("absent");

    assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
    clears_only_a_failed_run(&id, &without_gate(&fake), true);
    assert_eq!(standin_text(&id), None);
}

/// A run that did not fail has nothing to clear, and `reset-failed` of its unit would be refused.
#[skuld::test]
fn leaving_an_absent_id_whose_standin_run_did_not_fail_clears_nothing(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed_standin(&task_spec(&id, "/bin/a", SCHEDULE));
    let fake = gate_then([show("inactive", 0), reload()]);
    let mgr = over(&fake);

    let e = uninstall_absent(&mgr, &id, RunInFlight::Leave).expect_err("absent");

    assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
    clears_only_a_failed_run(&id, &without_gate(&fake), false);
    assert_eq!(without_gate(&fake).len(), 2, "{:?}", fake.calls());
    assert_eq!(standin_text(&id), None);
}

/// Without the choice it stops the run, then removes the file.
#[skuld::test]
fn stopping_an_absent_id_stops_the_run_of_its_standin_then_removes_it(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed_standin(&task_spec(&id, "/bin/a", SCHEDULE));
    let fake = gate_then([enqueued("stop", 3), show("inactive", 0), reload()]);
    let mgr = over(&fake);

    let e = uninstall_absent(&mgr, &id, RunInFlight::Stop).expect_err("absent");

    assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
    let calls = without_gate(&fake);
    assert_eq!(
        calls[0],
        call(&["stop", "--show-transaction", "--", &format!("{id}.service")])
    );
    assert_eq!(
        calls.len(),
        3,
        "the stop, the read of how it ended, the reload: {calls:?}"
    );
    clears_only_a_failed_run(&id, &calls, false);
    assert_eq!(standin_text(&id), None);
}

/// A file at the stand-in's path that is not goetia's is not goetia's to stop or remove.
#[skuld::test]
fn an_absent_ids_foreign_runtime_unit_is_left_alone(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let foreign = "[Unit]\nDescription=somebody else's runtime unit\n";
    std::fs::create_dir_all(RUNTIME_UNIT_DIR).expect("create the runtime unit directory");
    std::fs::write(standin_path(&id), foreign).expect("seed a foreign runtime unit");
    let fake = gate_then([]);
    let mgr = over(&fake);

    for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
        let e = uninstall_absent(&mgr, &id, in_flight).expect_err("absent");
        assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
    }

    assert!(without_gate(&fake).is_empty(), "{:?}", fake.calls());
    assert_eq!(standin_text(&id).as_deref(), Some(foreign));
}

// An absent id with a stand-in is not installed =======================================================================

/// The stand-in is a fragment under `/run/systemd/system`, and goetia reads fragments in `/etc/systemd/system`
/// alone: `read`, `list` and `install`'s ownership check all see an id nobody installed.
#[skuld::test]
fn an_absent_id_with_a_standin_reads_as_not_installed_everywhere(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", SCHEDULE);
    seed_standin(&spec);
    let mgr = over(&gate_then([]));

    assert!(
        matches!(
            Backend::read(&mgr, &Id::try_from(id.as_str()).unwrap()),
            Ok(ReadOutcome::Absent)
        ),
        "read"
    );
    let scan = Backend::scan(&mgr);
    assert!(scan.entries.iter().all(|(name, _)| *name != id), "list's entries");
    assert!(
        scan.reported.iter().all(|entry| !format!("{entry:?}").contains(&id)),
        "list's reports: {:?}",
        scan.reported
    );
    assert_eq!(
        flow::preview(&mgr, &spec).expect("preview"),
        Outcome::Create,
        "install's ownership check"
    );
}
