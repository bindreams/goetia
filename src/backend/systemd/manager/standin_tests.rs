//! `uninstall`'s choice of leaving a run in flight, on systemd: what each sequence does, the stand-in unit
//! it writes for a run still in its start job, and when that goes. Driven through a fake `systemctl`; the
//! files are real, so the tests join the `systemd_group` fixture.

use std::sync::Arc;

use super::super::systemctl::systemctl_tests::gate_then;
use super::super::task::task_tests::{
    Leftovers, call, enqueued, held, marked, over, reload, seed, service_spec, task_spec, text_of, units_of,
    without_gate,
};
use super::super::task::{Body, Fragment, Timer};
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

/// The one read of a unit's run: these properties, in this order.
const READ: &str = "ActiveState,SubState,KillSignal,TimeoutStopUSec,LoadState,FragmentPath";

fn read_call(id: &str) -> Vec<String> {
    call(&["show", "--property", READ, "--", &format!("{id}.service")])
}

fn etc_path(id: &str) -> String {
    unit_path(id, UnitKind::Service).display().to_string()
}

/// `systemctl show` of `id`'s job as the one read asks it, loaded from goetia's own path and stopping with the
/// defaults.
fn state(id: &str, active: &str, sub: &str) -> Entry {
    unit_state(active, sub, "loaded", &etc_path(id), "15", "1min 30s")
}

fn unit_state(active: &str, sub: &str, load: &str, fragment: &str, kill: &str, timeout: &str) -> Entry {
    reply(
        &["show"],
        exited(
            0,
            &format!(
                "ActiveState={active}\nSubState={sub}\nKillSignal={kill}\nTimeoutStopUSec={timeout}\n\
                 LoadState={load}\nFragmentPath={fragment}\n"
            ),
            "",
        ),
    )
}

/// `systemctl show` of a job systemd has no unit file for.
fn not_loaded() -> Entry {
    unit_state("inactive", "dead", "not-found", "", "15", "1min 30s")
}

fn reset_failed() -> Entry {
    reply(&["reset-failed"], exited(0, "", ""))
}

fn failed_reset(failed: bool) -> Vec<Entry> {
    if failed { vec![reset_failed()] } else { vec![] }
}

/// The read of a left run that is not active, and the `reset-failed` that follows the removal only where the
/// run failed: systemd refuses `reset-failed` of a unit that is not in memory, which one that did not fail is
/// not.
fn read_of(id: &str, failed: bool) -> Entry {
    if failed {
        state(id, "failed", "failed")
    } else {
        state(id, "inactive", "dead")
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

fn stop_call(id: &str) -> Vec<String> {
    call(&["stop", "--show-transaction", "--", &format!("{id}.service")])
}

fn disable_call(id: &str) -> Vec<String> {
    call(&["disable", "--", &format!("{id}.service")])
}

fn disable_timer_call(id: &str) -> Vec<String> {
    call(&["disable", "--now", "--", &format!("{id}.timer")])
}

/// What `generate::standin` writes for the values the read printed.
fn standin_of(id: &str, kill: &str, timeout: &str) -> String {
    generate::standin(id, kill, timeout)
}

/// What a read of `state`'s defaults leaves: the stand-in a start job gets.
fn default_standin(id: &str) -> String {
    standin_of(id, "15", "1min 30s")
}

/// A stand-in as an earlier `uninstall --no-stop` left it.
fn seed_standin(id: &str, kill: &str, timeout: &str) {
    let path = standin_path(id);
    std::fs::create_dir_all(path.parent().expect("a directory")).expect("create the runtime unit directory");
    std::fs::write(&path, standin_of(id, kill, timeout)).unwrap_or_else(|e| panic!("seed {}: {e}", path.display()));
}

/// A file at the stand-in's path that is not goetia's.
fn seed_foreign() -> &'static str {
    "[Unit]\nDescription=somebody else's runtime unit\n"
}

fn seed_foreign_at(id: &str) {
    std::fs::create_dir_all(RUNTIME_UNIT_DIR).expect("create the runtime unit directory");
    std::fs::write(standin_path(id), seed_foreign()).expect("seed a foreign runtime unit");
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

/// A job `read` found goetia's whose metadata does not decode, and that has no timer: it is not known to be
/// a task, and systemd's own report is all there is to go by.
fn undecodable_job(id: &str) -> Fragment {
    let text = units_of(&task_spec(id, "/bin/a", None)).service;
    seed(id, UnitKind::Service, &text);
    Fragment::new(
        Id::try_from(id).expect("an id"),
        Err(Arc::new(Error::blob("it does not decode"))),
        Body {
            service: Some(text),
            timer: None,
        },
    )
}

/// What `read` makes of a timer whose job is gone: the timer alone.
fn timer_alone(id: &str, spec: &DaemonSpec) -> Fragment {
    let text = units_of(spec).timer.expect("a task with a schedule");
    seed(id, UnitKind::Timer, &text);
    let blob = generate::extract(&text)
        .expect("a text that extracts")
        .expect("a marker");
    Fragment::new(
        Id::try_from(id).expect("an id"),
        Err(Arc::new(Error::blob("the job is missing"))),
        Body {
            service: None,
            timer: Some(Timer { text, blob: Ok(blob) }),
        },
    )
}

// What `Leave` does with the one read =================================================================================

/// A service whose run is past its start job needs no stand-in: every step of today's uninstall but the stop.
#[skuld::test]
fn leaving_a_running_service_does_every_step_but_the_stop(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed(&id, UnitKind::Service, &units_of(&service_spec(&id)).service);
    let fake = gate_then([state(&id, "active", "running"), disabled(), reload()]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("uninstall");

    assert_eq!(
        without_gate(&fake),
        [read_call(&id), disable_call(&id), call(&["daemon-reload"])]
    );
    assert!(!unit_path(&id, UnitKind::Service).exists());
    assert_eq!(standin_text(&id), None);
}

/// A service still in its start job gets the stand-in, carrying the two values the read printed.
#[skuld::test]
fn leaving_a_service_in_its_start_job_writes_a_standin_with_the_values_the_read_printed(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed(&id, UnitKind::Service, &units_of(&service_spec(&id)).service);
    let fake = gate_then([
        unit_state("activating", "start", "loaded", &etc_path(&id), "2", "5s"),
        disabled(),
        reload(),
    ]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("uninstall");

    assert_eq!(
        without_gate(&fake),
        [read_call(&id), disable_call(&id), call(&["daemon-reload"])],
        "no stop"
    );
    assert_eq!(standin_text(&id), Some(standin_of(&id, "2", "5s")));
    assert!(!unit_path(&id, UnitKind::Service).exists());
}

/// A service waiting to be restarted has no process: it is the plain uninstall, whose stop cancels the
/// restart and signals nothing.
#[skuld::test]
fn leaving_a_service_waiting_to_be_restarted_is_the_plain_uninstall(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed(&id, UnitKind::Service, &units_of(&service_spec(&id)).service);
    let fake = gate_then([
        state(&id, "activating", "auto-restart"),
        enqueued("stop", 3),
        disabled(),
        reload(),
    ]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("uninstall");

    assert_eq!(
        without_gate(&fake),
        [
            read_call(&id),
            stop_call(&id),
            disable_call(&id),
            call(&["daemon-reload"])
        ]
    );
    assert_eq!(standin_text(&id), None);
}

/// A state systemd reports that goetia does not know is a run, never "none": nothing is stopped.
#[skuld::test]
fn leaving_a_job_in_a_state_goetia_does_not_know_stops_and_writes_nothing(#[fixture(systemd_group)] _group: &Group) {
    for (active, sub) in [
        ("deactivating", "stop-sigterm"),
        ("reloading", "reload"),
        ("maintenance", "clean"),
        ("refreshing", "x"),
        ("a-state-of-the-future", "y"),
        ("active", "running"),
    ] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        installed(&id, SCHEDULE);
        let fake = gate_then([disabled(), state(&id, active, sub), reload()]);
        let mgr = over(&fake);
        let snapshot = marked(&mgr, &id);

        mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
            .expect("uninstall");

        assert_eq!(
            without_gate(&fake),
            [disable_timer_call(&id), read_call(&id), call(&["daemon-reload"])],
            "{active}"
        );
        assert_eq!(standin_text(&id), None, "{active}");
    }
}

/// Nothing running is a plain uninstall, with its stop: a run that is not there cannot be left.
#[skuld::test]
fn leaving_a_task_with_no_run_is_the_plain_uninstall_with_its_stop(#[fixture(systemd_group)] _group: &Group) {
    for state_of in ["inactive", "failed", "auto-restart", "not-loaded"] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        installed(&id, SCHEDULE);
        let read = match state_of {
            "inactive" => state(&id, "inactive", "dead"),
            "failed" => state(&id, "failed", "failed"),
            "auto-restart" => state(&id, "activating", "auto-restart"),
            _ => not_loaded(),
        };
        let stop = if state_of == "not-loaded" {
            reply(&["stop"], exited(5, "", "Unit not loaded."))
        } else {
            enqueued("stop", 3)
        };
        let fake = gate_then([disabled(), read, stop, reload()]);
        let mgr = over(&fake);
        let snapshot = marked(&mgr, &id);

        mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
            .expect("uninstall");

        assert_eq!(
            without_gate(&fake),
            [
                disable_timer_call(&id),
                read_call(&id),
                stop_call(&id),
                call(&["daemon-reload"])
            ],
            "{state_of}"
        );
        assert_eq!(standin_text(&id), None, "{state_of}");
        assert!(paths(&id).iter().all(|p| !p.exists()), "{state_of}");
    }
}

#[skuld::test]
fn leaving_a_task_in_its_start_job_writes_the_standin_before_the_removal_and_the_reload(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    installed(&id, SCHEDULE);
    let (before_removal, at_reload) = (paths(&id), paths(&id));
    let (standin_then, standin_now) = (standin_path(&id), standin_path(&id));
    let expected = standin_of(&id, "12", "45s");
    let fake = gate_then([
        disabled(),
        unit_state("activating", "start", "loaded", &etc_path(&id), "12", "45s").then(move || {
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
        [disable_timer_call(&id), read_call(&id), call(&["daemon-reload"])],
        "no stop, and the run is read once, after the timer is disarmed"
    );
    assert_eq!(
        generate::standin_id(&standin_text(&id).expect("a stand-in")).as_deref(),
        Some(id.as_str())
    );
}

#[skuld::test]
fn leaving_a_task_with_no_schedule_in_its_start_job_writes_the_standin_too(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    installed(&id, None);
    let fake = gate_then([state(&id, "activating", "start"), reload()]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("uninstall");

    assert_eq!(without_gate(&fake), [read_call(&id), call(&["daemon-reload"])]);
    assert_eq!(standin_text(&id), Some(default_standin(&id)));
    assert!(paths(&id).iter().all(|p| !p.exists()));
}

/// The decision is systemd's report that the run is in its start job, not goetia's reading of the job: one
/// whose metadata does not decode and that has no timer is kept alive too.
#[skuld::test]
fn leaving_a_job_whose_blob_does_not_decode_and_has_no_timer_writes_the_standin(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let snapshot = undecodable_job(&id);
    let fake = gate_then([state(&id, "activating", "start"), reload()]);
    let mgr = over(&fake);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("uninstall");

    assert!(
        without_gate(&fake).iter().all(|c| c[0] != "stop"),
        "{:?}",
        without_gate(&fake)
    );
    assert_eq!(standin_text(&id), Some(default_standin(&id)));
    assert!(!unit_path(&id, UnitKind::Service).exists());
}

/// A stand-in of ours already at the path is replaced: its values are the earlier job's, and this run's are
/// what the read printed now.
#[skuld::test]
fn leaving_a_job_in_its_start_job_replaces_a_stale_standin(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    installed(&id, SCHEDULE);
    seed_standin(&id, "9", "1s");
    let fake = gate_then([
        disabled(),
        unit_state("activating", "start", "loaded", &etc_path(&id), "3", "7s"),
        reload(),
    ]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("uninstall");

    assert_eq!(standin_text(&id), Some(standin_of(&id, "3", "7s")));
}

/// `Stop` stops the job and needs no stand-in: it is today's sequence, and the file is never written.
#[skuld::test]
fn stopping_a_task_writes_no_standin_and_reads_nothing(#[fixture(systemd_group)] _group: &Group) {
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
        [disable_timer_call(&id), stop_call(&id), call(&["daemon-reload"])]
    );
    assert_eq!(standin_text(&id), None);
}

// A file that is not goetia's at the stand-in's path ==================================================================

/// Refused before anything is changed, the timer included, whether or not a run is in flight: nothing is
/// asked of systemd at all.
#[skuld::test]
fn a_foreign_file_at_the_standins_path_refuses_leave_before_anything_is_touched(
    #[fixture(systemd_group)] _group: &Group,
) {
    for service in [false, true] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        if service {
            seed(&id, UnitKind::Service, &units_of(&service_spec(&id)).service);
        } else {
            installed(&id, SCHEDULE);
        }
        seed_foreign_at(&id);
        let fake = gate_then([]);
        let mgr = over(&fake);
        let snapshot = marked(&mgr, &id);

        let e = mgr
            .uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
            .expect_err("the stand-in's path is taken");

        assert!(
            matches!(&e, Error::StandinBlocked { id: named, path } if *named == id && *path == standin_path(&id)),
            "{e:?}"
        );
        assert!(
            without_gate(&fake).is_empty(),
            "the timer is not disarmed: {:?}",
            fake.calls()
        );
        assert_eq!(
            standin_text(&id).as_deref(),
            Some(seed_foreign()),
            "the foreign file is untouched"
        );
        assert!(
            unit_path(&id, UnitKind::Service).exists(),
            "nothing of the daemon was removed"
        );
    }
}

/// Not a regular file either: a unit masked by a link to `/dev/null` is somebody's.
#[skuld::test]
fn a_masked_runtime_unit_at_the_standins_path_refuses_leave_too(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    installed(&id, SCHEDULE);
    std::fs::create_dir_all(RUNTIME_UNIT_DIR).expect("create the runtime unit directory");
    std::os::unix::fs::symlink("/dev/null", standin_path(&id)).expect("mask the runtime unit");
    let fake = gate_then([]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    let e = mgr
        .uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect_err("the stand-in's path is taken");

    assert!(matches!(e, Error::StandinBlocked { .. }), "{e:?}");
    assert!(without_gate(&fake).is_empty(), "{:?}", fake.calls());
}

/// `Stop` writes nothing there and is not in its way.
#[skuld::test]
fn a_foreign_file_at_the_standins_path_does_not_stop_a_stop(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    installed(&id, SCHEDULE);
    seed_foreign_at(&id);
    let fake = gate_then([disabled(), enqueued("stop", 3), reload()]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, RunInFlight::Stop, &mut held(&mgr))
        .expect("uninstall");

    assert_eq!(standin_text(&id).as_deref(), Some(seed_foreign()));
}

/// `ConcurrentChange` is what a file that appears after the check is: the one place left that raises it.
#[skuld::test]
fn a_foreign_file_that_appears_after_the_check_is_a_concurrent_change(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    installed(&id, SCHEDULE);
    let appears = id.clone();
    let fake = gate_then([
        disabled(),
        state(&id, "activating", "start").then(move || seed_foreign_at(&appears)),
    ]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    let e = mgr
        .uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect_err("a file appeared");

    assert!(
        matches!(&e, Error::ConcurrentChange { path: Some(path), .. } if *path == standin_path(&id)),
        "{e:?}"
    );
    assert!(paths(&id).iter().all(|p| p.exists()), "nothing was removed");
}

// The one rule for a stand-in of ours that is found ===================================================================
//
// The same three questions of each caller: an `uninstall` of an installed id, of an absent id, and an
// `install`. `Stop` stops the run first, then removes; `Leave` removes it when the run is none, after
// `reset-failed` where the read says `failed`, and leaves it with a run in flight.

/// `uninstall` of an installed id.
#[skuld::test]
fn an_uninstall_removes_a_standin_when_the_run_is_none(#[fixture(systemd_group)] _group: &Group) {
    for failed in [false, true] {
        for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
            let id = fresh_id("systemd");
            let _left = Leftovers::new(&id);
            installed(&id, SCHEDULE);
            seed_standin(&id, "15", "1min 30s");
            // `Leave` reads first and stops a run that is none; `Stop` stops first and reads what is left.
            let mut steps = vec![disabled()];
            match in_flight {
                RunInFlight::Stop => steps.extend([enqueued("stop", 3), read_of(&id, failed)]),
                RunInFlight::Leave => steps.extend([read_of(&id, failed), enqueued("stop", 3)]),
            }
            steps.extend(failed_reset(failed));
            steps.push(reload());
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

/// With `Stop` the run is stopped whenever there is a stand-in, whatever the snapshot holds: a timer and no
/// job still has a run to stop.
#[skuld::test]
fn stopping_a_snapshot_with_a_timer_and_no_job_still_stops_the_run_of_its_standin(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", SCHEDULE);
    let snapshot = timer_alone(&id, &spec);
    seed_standin(&id, "15", "1min 30s");
    let fake = gate_then([disabled(), enqueued("stop", 3), read_of(&id, false), reload()]);
    let mgr = over(&fake);

    mgr.uninstall_units(snapshot, RunInFlight::Stop, &mut held(&mgr))
        .expect("uninstall");

    assert_eq!(
        without_gate(&fake),
        [
            disable_timer_call(&id),
            stop_call(&id),
            read_call(&id),
            call(&["daemon-reload"])
        ]
    );
    assert_eq!(standin_text(&id), None);
    assert!(paths(&id).iter().all(|p| !p.exists()));
}

/// With a run still going, `Leave` does not remove it: a run that is past its start job (the job is the
/// installed unit's, which outranks the stand-in) leaves it where it is.
#[skuld::test]
fn an_uninstall_leaves_a_standin_with_a_run_going(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    installed(&id, SCHEDULE);
    seed_standin(&id, "15", "1min 30s");
    let fake = gate_then([disabled(), state(&id, "active", "running"), reload()]);
    let mgr = over(&fake);
    let snapshot = marked(&mgr, &id);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("uninstall");

    assert!(
        !without_gate(&fake)
            .iter()
            .any(|c| c[0] == "reset-failed" || c[0] == "stop")
    );
    assert_eq!(standin_text(&id), Some(default_standin(&id)));
}

/// `uninstall` of an absent id.
#[skuld::test]
fn uninstalling_an_absent_id_removes_a_standin_when_the_run_is_none(#[fixture(systemd_group)] _group: &Group) {
    for failed in [false, true] {
        for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
            let id = fresh_id("systemd");
            let _left = Leftovers::new(&id);
            seed_standin(&id, "15", "1min 30s");
            let mut steps = vec![];
            if in_flight == RunInFlight::Stop {
                steps.push(enqueued("stop", 3));
            }
            steps.push(read_of(&id, failed));
            steps.extend(failed_reset(failed));
            steps.push(reload());
            let fake = gate_then(steps);
            let mgr = over(&fake);

            let e = uninstall_absent(&mgr, &id, in_flight).expect_err("absent");

            assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
            let calls = without_gate(&fake);
            clears_only_a_failed_run(&id, &calls, failed);
            assert_eq!(
                calls.first(),
                Some(&if in_flight == RunInFlight::Stop {
                    stop_call(&id)
                } else {
                    read_call(&id)
                })
            );
            assert_eq!(standin_text(&id), None, "{in_flight:?}, failed: {failed}");
        }
    }
}

/// `Leave` leaves a stand-in whose run is in flight, and the run with it: one read and nothing else.
#[skuld::test]
fn leaving_an_absent_id_whose_standin_has_a_run_in_flight_changes_nothing(#[fixture(systemd_group)] _group: &Group) {
    for (active, sub) in [("activating", "start"), ("active", "running")] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        seed_standin(&id, "15", "1min 30s");
        let from = standin_path(&id).display().to_string();
        let fake = gate_then([unit_state(active, sub, "loaded", &from, "15", "1min 30s")]);
        let mgr = over(&fake);

        let e = uninstall_absent(&mgr, &id, RunInFlight::Leave).expect_err("absent");

        assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
        assert_eq!(without_gate(&fake), [read_call(&id)], "{active}");
        assert_eq!(standin_text(&id), Some(default_standin(&id)));
    }
}

/// `install` of the id.
#[skuld::test]
fn an_install_removes_a_standin_when_the_run_is_none(#[fixture(systemd_group)] _group: &Group) {
    for failed in [false, true] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        let spec = task_spec(&id, "/bin/a", SCHEDULE);
        seed_standin(&id, "15", "1min 30s");
        let mut steps = vec![read_of(&id, failed)];
        steps.extend(failed_reset(failed));
        steps.extend([reload(), reload()]);
        let fake = gate_then(steps);
        let mgr = over(&fake);

        let outcome = install(&mgr, &spec);

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
/// the process under the new definition. It never stops the run.
#[skuld::test]
fn an_install_over_a_left_run_leaves_the_standin_and_is_not_refused(#[fixture(systemd_group)] _group: &Group) {
    for (active, sub) in [("activating", "start"), ("active", "running")] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        let spec = task_spec(&id, "/bin/a", SCHEDULE);
        seed_standin(&id, "15", "1min 30s");
        let fake = gate_then([state(&id, active, sub), reload()]);
        let mgr = over(&fake);

        let outcome = install(&mgr, &spec);

        assert_eq!(outcome, Outcome::Create);
        assert!(
            !without_gate(&fake)
                .iter()
                .any(|c| c[0] == "stop" || c[0] == "reset-failed")
        );
        assert_eq!(standin_text(&id), Some(default_standin(&id)), "{active}");
    }
}

fn install(mgr: &Systemd, spec: &DaemonSpec) -> Outcome {
    flow::install(
        mgr,
        spec,
        false,
        &mut Backend::reserve(mgr, &[crate::manager::Step::Install], crate::manager::Budget::Unbounded)
            .expect("reserve"),
    )
    .expect("install")
}

// The absent id =======================================================================================================

/// Nothing of goetia's at the id and nothing loaded: one read, whichever choice, and the answer is "not
/// installed".
#[skuld::test]
fn uninstalling_an_absent_id_with_nothing_held_reads_once_and_changes_nothing(
    #[fixture(systemd_group)] _group: &Group,
) {
    for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        let fake = gate_then([not_loaded()]);
        let mgr = over(&fake);

        let e = uninstall_absent(&mgr, &id, in_flight).expect_err("absent");

        assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
        assert_eq!(without_gate(&fake), [read_call(&id)], "{in_flight:?}");
    }
}

/// systemd holds a definition loaded from goetia's own path whose file is gone (a `Leave` uninstall whose
/// reload failed): the reload is made before the answer, whichever the choice.
#[skuld::test]
fn uninstalling_an_absent_id_whose_definition_systemd_still_holds_reloads(#[fixture(systemd_group)] _group: &Group) {
    for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        let fake = gate_then([state(&id, "active", "running"), reload()]);
        let mgr = over(&fake);

        let e = uninstall_absent(&mgr, &id, in_flight).expect_err("absent");

        assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
        assert_eq!(
            without_gate(&fake),
            [read_call(&id), call(&["daemon-reload"])],
            "{in_flight:?}"
        );
    }
}

/// A reload that fails is the call's error: "not installed" would say the definition is gone.
#[skuld::test]
fn a_failed_reload_for_a_held_definition_is_the_calls_error(#[fixture(systemd_group)] _group: &Group) {
    for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        let fake = gate_then([
            state(&id, "active", "running"),
            reply(&["daemon-reload"], exited(1, "", "Failed to reload")),
        ]);
        let mgr = over(&fake);

        let e = uninstall_absent(&mgr, &id, in_flight).expect_err("the reload failed");

        assert!(matches!(e, Error::Command { .. }), "{in_flight:?}: {e:?}");
    }
}

/// Only a definition loaded from goetia's own path counts: one from another path, or not loaded, is not
/// ours to reload.
#[skuld::test]
fn a_definition_from_another_path_or_none_is_not_reloaded(#[fixture(systemd_group)] _group: &Group) {
    for (load, fragment) in [
        ("loaded", "/usr/lib/systemd/system/x.service"),
        ("loaded", "/run/systemd/system/x.service"),
        ("not-found", ""),
        ("masked", "/dev/null"),
    ] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        let fake = gate_then([unit_state("active", "running", load, fragment, "15", "1min 30s")]);
        let mgr = over(&fake);

        let e = uninstall_absent(&mgr, &id, RunInFlight::Leave).expect_err("absent");

        assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
        assert_eq!(without_gate(&fake), [read_call(&id)], "{load} {fragment}");
    }
}

/// A stand-in that stays, with the definition still held from goetia's path (the retry after a failed
/// reload of a `Leave` that wrote it): the reload is made and the stand-in and its run stay.
#[skuld::test]
fn a_retry_after_a_failed_reload_reloads_and_leaves_the_standin_and_its_run(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed_standin(&id, "15", "1min 30s");
    let fake = gate_then([state(&id, "activating", "start"), reload()]);
    let mgr = over(&fake);

    let e = uninstall_absent(&mgr, &id, RunInFlight::Leave).expect_err("absent");

    assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
    assert_eq!(without_gate(&fake), [read_call(&id), call(&["daemon-reload"])]);
    assert_eq!(standin_text(&id), Some(default_standin(&id)));
}

/// A stand-in removed and a definition held make one reload.
#[skuld::test]
fn a_removed_standin_and_a_held_definition_make_one_reload(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed_standin(&id, "15", "1min 30s");
    let fake = gate_then([state(&id, "inactive", "dead"), reload()]);
    let mgr = over(&fake);

    let e = uninstall_absent(&mgr, &id, RunInFlight::Leave).expect_err("absent");

    assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
    assert_eq!(without_gate(&fake), [read_call(&id), call(&["daemon-reload"])]);
    assert_eq!(standin_text(&id), None);
}

/// A file at the stand-in's path that is not goetia's is not goetia's to stop or remove: the read is made, the
/// file is untouched.
#[skuld::test]
fn an_absent_ids_foreign_runtime_unit_is_left_alone(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed_foreign_at(&id);
    let fake = gate_then([not_loaded(), not_loaded()]);
    let mgr = over(&fake);

    for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
        let e = uninstall_absent(&mgr, &id, in_flight).expect_err("absent");
        assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
    }

    assert!(without_gate(&fake).iter().all(|c| c[0] == "show"), "{:?}", fake.calls());
    assert_eq!(standin_text(&id).as_deref(), Some(seed_foreign()));
}

// An absent id with a stand-in is not installed =======================================================================

/// The stand-in is a fragment under `/run/systemd/system`, and goetia reads fragments in `/etc/systemd/system`
/// alone: `read`, `list` and `install`'s ownership check all see an id nobody installed.
#[skuld::test]
fn an_absent_id_with_a_standin_reads_as_not_installed_everywhere(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", SCHEDULE);
    seed_standin(&id, "15", "1min 30s");
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
