//! `uninstall` and `install` over a run in flight and a stand-in, on systemd: the order `Systemd::settle` carries
//! a plan out in, and what each of its three callers reads and sends. Which plan each combination gets is
//! `plan_tests`'s; these show the calls that plan comes to, driven through a fake `systemctl`. The files are
//! real, so the tests join the `systemd_group` fixture.

use std::sync::Arc;

use super::super::busctl::Busctl;
use super::super::journal::fake::Fake as JournalFake;
use super::super::systemctl::Systemctl;
use super::super::systemctl::systemctl_tests::{booted, gate_passes, gate_then, unbooted};
use super::super::task::task_tests::{
    Leftovers, call, enqueued, held, marked, over, reload, seed, service_spec, task_spec, text_of, units_of,
};
use super::super::task::{Body, Fragment, Timer};
use super::super::{Systemd, unit_path};
use super::*;
use crate::backend::client::fake::{Entry, Fake, exited, reply};
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

/// The read of a left run with no process, and the `reset-failed` that follows only where the read says it failed:
/// systemd refuses `reset-failed` of a unit that is not in memory, which one that did not fail is not.
fn read_of(id: &str, failed: bool) -> Entry {
    if failed {
        state(id, "failed", "failed")
    } else {
        state(id, "inactive", "dead")
    }
}

/// A fake whose first calls are `reads` (a read asks no gate), then the version gate that the first request
/// draws, then `rest`.
fn read_then_gate(reads: impl IntoIterator<Item = Entry>, rest: impl IntoIterator<Item = Entry>) -> Fake {
    Fake::new(reads.into_iter().chain(gate_passes()).chain(rest))
}

/// Every call, with the version gate's two probes taken out wherever they fall.
fn calls_of(fake: &Fake) -> Vec<Vec<String>> {
    fake.calls()
        .into_iter()
        .filter(|c| !(c == &call(&["--version"]) || c.starts_with(&call(&["show", "--property=Version"]))))
        .collect()
}

fn reload_call() -> Vec<String> {
    call(&["daemon-reload"])
}

fn reset_call(id: &str) -> Vec<String> {
    call(&["reset-failed", "--", &format!("{id}.service")])
}

/// `calls` hold the `reset-failed` of `id` only if the run `failed`, and it comes before the reload.
fn clears_only_a_failed_run(id: &str, calls: &[Vec<String>], failed: bool) {
    let (reset, reload) = (reset_call(id), reload_call());
    assert_eq!(calls.contains(&reset), failed, "{calls:?}");
    if failed {
        let at = |wanted: &Vec<String>| calls.iter().position(|seen| seen == wanted);
        assert!(at(&reset) < at(&reload), "before the reload: {calls:?}");
    }
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

fn never_stopped(calls: &[Vec<String>]) {
    assert!(calls.iter().all(|c| c[0] != "stop"), "{calls:?}");
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

/// A manager whose `systemctl` is over `fake` on a host shown by `host`.
fn over_host(fake: &Fake, host: fn() -> super::super::systemctl::Host) -> Systemd {
    Systemd::with_seams(
        Systemctl::new(Box::new(fake.clone()), host),
        Busctl::new(Box::new(Fake::new([])), booted),
        Arc::new(JournalFake::new(Vec::new())),
    )
}

// Leave of an installed id ============================================================================================

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

    assert_eq!(calls_of(&fake), [read_call(&id), disable_call(&id), reload_call()]);
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
        calls_of(&fake),
        [read_call(&id), disable_call(&id), reload_call()],
        "no stop"
    );
    assert_eq!(standin_text(&id), Some(standin_of(&id, "2", "5s")));
    assert!(!unit_path(&id, UnitKind::Service).exists());
}

/// A service whose restart is waiting is no run goetia can end without a race, and gets no stand-in either: it
/// sends nothing, clears nothing, and reads nothing once the reload is done. The reload fails the restart on
/// the missing unit; what that leaves is the README's.
#[skuld::test]
fn leaving_a_service_whose_restart_is_waiting_sends_nothing_and_clears_nothing(
    #[fixture(systemd_group)] _group: &Group,
) {
    for sub in ["auto-restart", "auto-restart-queued"] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        seed(&id, UnitKind::Service, &units_of(&service_spec(&id)).service);
        let fake = gate_then([state(&id, "activating", sub), disabled(), reload()]);
        let mgr = over(&fake);
        let snapshot = marked(&mgr, &id);

        mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
            .expect("uninstall");

        assert_eq!(
            calls_of(&fake),
            [read_call(&id), disable_call(&id), reload_call()],
            "{sub}: no stop, no reset-failed, no read after the reload"
        );
        assert_eq!(standin_text(&id), None, "{sub}: no stand-in");
        assert!(!unit_path(&id, UnitKind::Service).exists(), "{sub}");
    }
}

/// A state systemd reports that goetia does not know is a run: no stop, no stand-in, whatever else is
/// removed.
#[skuld::test]
fn a_task_in_a_state_goetia_does_not_know_is_a_run_that_gets_no_stop_and_no_standin(
    #[fixture(systemd_group)] _group: &Group,
) {
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
            calls_of(&fake),
            [disable_timer_call(&id), read_call(&id), reload_call()],
            "{active}"
        );
        assert_eq!(standin_text(&id), None, "{active}");
    }
}

/// `Leave` never sends a stop, in any state, a unit with no process included: nothing is there to stop.
#[skuld::test]
fn leaving_a_task_with_no_process_sends_no_stop(#[fixture(systemd_group)] _group: &Group) {
    for state_of in ["inactive", "failed", "not-loaded"] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        installed(&id, SCHEDULE);
        let read = match state_of {
            "inactive" => state(&id, "inactive", "dead"),
            "failed" => state(&id, "failed", "failed"),
            _ => not_loaded(),
        };
        let fake = gate_then([disabled(), read, reload()]);
        let mgr = over(&fake);
        let snapshot = marked(&mgr, &id);

        mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
            .expect("uninstall");

        assert_eq!(
            calls_of(&fake),
            [disable_timer_call(&id), read_call(&id), reload_call()],
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
        calls_of(&fake),
        [disable_timer_call(&id), read_call(&id), reload_call()],
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

    assert_eq!(calls_of(&fake), [read_call(&id), reload_call()]);
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
    // Its kind is not known, so it is disabled like a service.
    let fake = gate_then([state(&id, "activating", "start"), disabled(), reload()]);
    let mgr = over(&fake);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("uninstall");

    never_stopped(&calls_of(&fake));
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

/// A timer alone is not a job goetia removes: its unit, if systemd has one, may be somebody else's, loaded from
/// elsewhere. No stand-in is written for it, whatever the unit is doing.
#[skuld::test]
fn leaving_a_timer_alone_whose_unit_is_in_its_start_job_writes_no_standin(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", SCHEDULE);
    let snapshot = timer_alone(&id, &spec);
    let fake = gate_then([disabled(), state(&id, "activating", "start"), reload()]);
    let mgr = over(&fake);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("uninstall");

    assert_eq!(
        calls_of(&fake),
        [disable_timer_call(&id), read_call(&id), reload_call()]
    );
    assert_eq!(standin_text(&id), None);
    assert!(paths(&id).iter().all(|p| !p.exists()));
}

/// An installed id passes the version gate before its first change, whichever the choice and whether or not it has
/// a timer to disarm: an unscheduled task under `Leave` must not meet a refusing gate at the final reload, after
/// its files are gone.
#[skuld::test]
fn an_installed_id_is_refused_by_the_gate_before_anything_is_removed(#[fixture(systemd_group)] _group: &Group) {
    for in_flight in [RunInFlight::Leave, RunInFlight::Stop] {
        for schedule in [None, SCHEDULE] {
            let id = fresh_id("systemd");
            let _left = Leftovers::new(&id);
            installed(&id, schedule);
            let fake = Fake::new([
                reply(&["--version"], exited(0, "systemd 241 (241)\n", "")),
                reply(&["show", "--property=Version", "--value"], exited(0, "241\n", "")),
            ]);
            let mgr = over(&fake);
            let snapshot = marked(&mgr, &id);

            let e = mgr
                .uninstall_units(snapshot, in_flight, &mut held(&mgr))
                .expect_err("the gate refuses");

            assert!(e.to_string().contains("systemd 243 or newer"), "{in_flight:?}: {e}");
            assert!(
                calls_of(&fake).is_empty(),
                "{in_flight:?}: nothing but the gate is asked: {:?}",
                fake.calls()
            );
            assert!(
                paths(&id).iter().filter(|p| p.exists()).count() == usize::from(schedule.is_some()) + 1,
                "{in_flight:?}: nothing was removed"
            );
            assert_eq!(standin_text(&id), None, "{in_flight:?}");
        }
    }
}

// Stop ================================================================================================================

/// `Stop` stops the job and needs no stand-in: it is today's sequence, with no read and no file written.
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
        calls_of(&fake),
        [disable_timer_call(&id), stop_call(&id), reload_call()]
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
            calls_of(&fake).is_empty(),
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

/// No stand-in is ever written for a timer alone, so a file at the stand-in's path is no concern of it: it is not
/// refused, and the file is left as it is.
#[skuld::test]
fn a_foreign_file_at_the_standins_path_does_not_refuse_leave_of_a_timer_alone(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let spec = task_spec(&id, "/bin/a", SCHEDULE);
    let snapshot = timer_alone(&id, &spec);
    seed_foreign_at(&id);
    let fake = gate_then([disabled(), state(&id, "activating", "start"), reload()]);
    let mgr = over(&fake);

    mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect("a timer alone is not refused");

    assert_eq!(
        calls_of(&fake),
        [disable_timer_call(&id), read_call(&id), reload_call()]
    );
    assert_eq!(standin_text(&id).as_deref(), Some(seed_foreign()), "the file is untouched");
    assert!(paths(&id).iter().all(|p| !p.exists()), "the timer is removed");
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
    assert!(calls_of(&fake).is_empty(), "{:?}", fake.calls());
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

// A stand-in of ours that is found, from each caller ==================================================================
//
// The same questions of an `uninstall` of an installed id, of an absent id, and an `install`: which plan each
// combination gets is `plan_tests`'s; here each caller reads once and sends what the plan says.

/// `uninstall` of an installed id with `Leave`: nothing is sent, and the stand-in of a run with no process is
/// removed, after `reset-failed` where the one read says it failed.
#[skuld::test]
fn leaving_an_installed_id_removes_a_standin_when_the_run_has_no_process(#[fixture(systemd_group)] _group: &Group) {
    for failed in [false, true] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        installed(&id, SCHEDULE);
        seed_standin(&id, "15", "1min 30s");
        let mut steps = vec![disabled(), read_of(&id, failed)];
        steps.extend(failed_reset(failed));
        steps.push(reload());
        let fake = gate_then(steps);
        let mgr = over(&fake);
        let snapshot = marked(&mgr, &id);

        mgr.uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
            .expect("uninstall");

        let calls = calls_of(&fake);
        clears_only_a_failed_run(&id, &calls, failed);
        assert_eq!(calls.last(), Some(&reload_call()), "{calls:?}");
        never_stopped(&calls);
        assert_eq!(standin_text(&id), None, "failed: {failed}");
        assert!(paths(&id).iter().all(|p| !p.exists()));
    }
}

/// `uninstall` of an installed id with `Stop` and a stand-in of ours: the stop comes first, and what the stop left
/// is read after it; `reset-failed` is sent when that read says failed, and the read before the stop, whatever
/// it said, decides nothing.
#[skuld::test]
fn stopping_an_installed_id_reads_whether_the_run_failed_after_the_stop(#[fixture(systemd_group)] _group: &Group) {
    for (before, after) in [(false, false), (true, false), (false, true), (true, true)] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        installed(&id, SCHEDULE);
        seed_standin(&id, "15", "1min 30s");
        let mut steps = vec![
            disabled(),
            state(&id, if before { "failed" } else { "active" }, if before { "failed" } else { "running" }),
            enqueued("stop", 3),
            read_of(&id, after),
        ];
        steps.extend(failed_reset(after));
        steps.push(reload());
        let fake = gate_then(steps);
        let mgr = over(&fake);
        let snapshot = marked(&mgr, &id);

        mgr.uninstall_units(snapshot, RunInFlight::Stop, &mut held(&mgr))
            .expect("uninstall");

        let mut expected = vec![
            disable_timer_call(&id),
            read_call(&id),
            stop_call(&id),
            read_call(&id),
        ];
        if after {
            expected.push(reset_call(&id));
        }
        expected.push(reload_call());
        assert_eq!(calls_of(&fake), expected, "before: {before}, after: {after}");
        assert_eq!(standin_text(&id), None, "before: {before}, after: {after}");
        assert!(paths(&id).iter().all(|p| !p.exists()));
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
    let fake = gate_then([
        disabled(),
        state(&id, "active", "running"),
        enqueued("stop", 3),
        state(&id, "inactive", "dead"),
        reload(),
    ]);
    let mgr = over(&fake);

    mgr.uninstall_units(snapshot, RunInFlight::Stop, &mut held(&mgr))
        .expect("uninstall");

    assert_eq!(
        calls_of(&fake),
        [
            disable_timer_call(&id),
            read_call(&id),
            stop_call(&id),
            read_call(&id),
            reload_call()
        ]
    );
    assert_eq!(standin_text(&id), None);
    assert!(paths(&id).iter().all(|p| !p.exists()));
}

/// With a run still going, `Leave` does not remove it, and sends it nothing.
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

    assert!(!calls_of(&fake).iter().any(|c| c[0] == "reset-failed" || c[0] == "stop"));
    assert_eq!(standin_text(&id), Some(default_standin(&id)));
}

/// `uninstall` of an absent id: `Stop` stops the stand-in's run and reads after the stop whether it failed,
/// `Leave` sends nothing and goes by the one read; the first read is made before any request, so it needs no
/// version gate.
#[skuld::test]
fn uninstalling_an_absent_id_removes_a_standin_when_the_run_has_no_process(#[fixture(systemd_group)] _group: &Group) {
    for failed in [false, true] {
        for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
            let id = fresh_id("systemd");
            let _left = Leftovers::new(&id);
            seed_standin(&id, "15", "1min 30s");
            let mut steps = vec![];
            if in_flight == RunInFlight::Stop {
                steps.extend([enqueued("stop", 3), read_of(&id, failed)]);
            }
            steps.extend(failed_reset(failed));
            steps.push(reload());
            // Under `Stop` the first read is of a run still going, whatever the stop leaves.
            let first = if in_flight == RunInFlight::Stop {
                state(&id, "active", "running")
            } else {
                read_of(&id, failed)
            };
            let fake = read_then_gate([first], steps);
            let mgr = over(&fake);

            let e = uninstall_absent(&mgr, &id, in_flight).expect_err("absent");

            assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
            let calls = calls_of(&fake);
            clears_only_a_failed_run(&id, &calls, failed);
            assert_eq!(calls.first(), Some(&read_call(&id)), "the read comes first");
            assert_eq!(calls.last(), Some(&reload_call()));
            assert_eq!(calls.contains(&stop_call(&id)), in_flight == RunInFlight::Stop);
            if in_flight == RunInFlight::Stop {
                let at = |wanted: &Vec<String>| calls.iter().rposition(|seen| seen == wanted);
                assert!(at(&stop_call(&id)) < at(&read_call(&id)), "read after the stop: {calls:?}");
            }
            assert_eq!(standin_text(&id), None, "{in_flight:?}, failed: {failed}");
        }
    }
}

/// `Leave` leaves a stand-in whose run has a process, and the run with it: one read and nothing else.
#[skuld::test]
fn leaving_an_absent_id_whose_standin_has_a_run_in_flight_changes_nothing(#[fixture(systemd_group)] _group: &Group) {
    for (active, sub) in [("activating", "start"), ("active", "running")] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        seed_standin(&id, "15", "1min 30s");
        let from = standin_path(&id).display().to_string();
        let fake = read_then_gate([unit_state(active, sub, "loaded", &from, "15", "1min 30s")], []);
        let mgr = over(&fake);

        let e = uninstall_absent(&mgr, &id, RunInFlight::Leave).expect_err("absent");

        assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
        assert_eq!(calls_of(&fake), [read_call(&id)], "{active}");
        assert_eq!(standin_text(&id), Some(default_standin(&id)));
    }
}

/// `install` of the id.
#[skuld::test]
fn an_install_removes_a_standin_when_the_run_has_no_process(#[fixture(systemd_group)] _group: &Group) {
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
        let calls = calls_of(&fake);
        assert_eq!(calls.len(), 3 + usize::from(failed), "{calls:?}");
        assert_eq!(calls.contains(&reset_call(&id)), failed);
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
        assert!(!calls_of(&fake).iter().any(|c| c[0] == "stop" || c[0] == "reset-failed"));
        assert_eq!(standin_text(&id), Some(default_standin(&id)), "{active}");
    }
}

/// With no stand-in of ours an `install` asks nothing of systemd about a run: only the units are written.
#[skuld::test]
fn an_install_with_no_standin_of_ours_reads_nothing(#[fixture(systemd_group)] _group: &Group) {
    for foreign in [false, true] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        let spec = task_spec(&id, "/bin/a", SCHEDULE);
        if foreign {
            seed_foreign_at(&id);
        }
        let fake = gate_then([reload()]);
        let mgr = over(&fake);

        install(&mgr, &spec);

        assert_eq!(calls_of(&fake), [reload_call()], "foreign: {foreign}");
        assert_eq!(standin_text(&id).is_some(), foreign);
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
        let fake = read_then_gate([not_loaded()], []);
        let mgr = over(&fake);

        let e = uninstall_absent(&mgr, &id, in_flight).expect_err("absent");

        assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
        assert_eq!(calls_of(&fake), [read_call(&id)], "{in_flight:?}");
        assert!(
            fake.calls().len() == 1,
            "no version gate for a read: {:?}",
            fake.calls()
        );
    }
}

/// Where no manager can be asked nothing is read, and the answer is "not installed", as it was before a
/// definition systemd holds could be asked about.
#[skuld::test]
fn uninstalling_an_absent_id_where_no_manager_can_be_asked_reads_nothing(#[fixture(systemd_group)] _group: &Group) {
    for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        let fake = Fake::new([]);
        let mgr = over_host(&fake, unbooted);

        let e = uninstall_absent(&mgr, &id, in_flight).expect_err("absent");

        assert!(matches!(e, Error::NotInstalled { .. }), "{in_flight:?}: {e:?}");
        assert!(fake.calls().is_empty(), "{in_flight:?}: {:?}", fake.calls());
    }
}

/// A host that shows nothing but whose `systemctl` says it ignored the request is a host with no manager too.
#[skuld::test]
fn uninstalling_an_absent_id_where_systemctl_says_it_asked_no_manager_is_not_installed(
    #[fixture(systemd_group)] _group: &Group,
) {
    for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        let fake = Fake::new([reply(
            &["show"],
            exited(0, "", "Running in chroot, ignoring request: show\n"),
        )]);
        let mgr = over(&fake);

        let e = uninstall_absent(&mgr, &id, in_flight).expect_err("absent");

        assert!(matches!(e, Error::NotInstalled { .. }), "{in_flight:?}: {e:?}");
        assert_eq!(calls_of(&fake), [read_call(&id)], "{in_flight:?}");
    }
}

/// An id that is installed asks the manager as every `uninstall` does: no manager is the refusal, not an
/// answer of "not installed".
#[skuld::test]
fn uninstalling_an_installed_id_where_no_manager_can_be_asked_is_refused(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    installed(&id, None);
    let fake = Fake::new([]);
    let mgr = over_host(&fake, unbooted);
    let snapshot = marked(&mgr, &id);

    let e = mgr
        .uninstall_units(snapshot, RunInFlight::Leave, &mut held(&mgr))
        .expect_err("no manager");

    assert!(matches!(e, Error::NoManager { .. }), "{e:?}");
    assert!(unit_path(&id, UnitKind::Service).exists(), "nothing was removed");
}

/// systemd holds a definition loaded from goetia's own path whose file is gone (a `Leave` uninstall whose
/// reload failed): the reload is made before the answer, whichever the choice, and nothing is stopped or
/// written for it, whatever state it is in.
#[skuld::test]
fn uninstalling_an_absent_id_whose_definition_systemd_still_holds_reloads_and_nothing_else(
    #[fixture(systemd_group)] _group: &Group,
) {
    for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
        for (active, sub) in [
            ("active", "running"),
            ("activating", "start"),
            ("activating", "auto-restart"),
            ("inactive", "dead"),
        ] {
            let id = fresh_id("systemd");
            let _left = Leftovers::new(&id);
            let fake = read_then_gate([state(&id, active, sub)], [reload()]);
            let mgr = over(&fake);

            let e = uninstall_absent(&mgr, &id, in_flight).expect_err("absent");

            assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
            assert_eq!(
                calls_of(&fake),
                [read_call(&id), reload_call()],
                "{in_flight:?} {active} {sub}"
            );
            assert_eq!(standin_text(&id), None);
        }
    }
}

/// A reload that fails is the call's error: "not installed" would say the definition is gone.
#[skuld::test]
fn a_failed_reload_for_a_held_definition_is_the_calls_error(#[fixture(systemd_group)] _group: &Group) {
    for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
        let id = fresh_id("systemd");
        let _left = Leftovers::new(&id);
        let fake = read_then_gate(
            [state(&id, "active", "running")],
            [reply(&["daemon-reload"], exited(1, "", "Failed to reload"))],
        );
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
        let fake = read_then_gate([unit_state("active", "running", load, fragment, "15", "1min 30s")], []);
        let mgr = over(&fake);

        let e = uninstall_absent(&mgr, &id, RunInFlight::Leave).expect_err("absent");

        assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
        assert_eq!(calls_of(&fake), [read_call(&id)], "{load} {fragment}");
    }
}

/// Held only when the file is gone: a definition loaded from the id's path whose file is there is not a
/// definition held for a file that is gone, and is not reloaded.
#[skuld::test]
fn a_definition_whose_file_is_there_is_not_held_for_a_file_that_is_gone(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed(&id, UnitKind::Service, "[Unit]\nDescription=somebody else's\n");
    let fake = read_then_gate([state(&id, "active", "running")], []);
    let mgr = over(&fake);

    let result = Backend::uninstall_absent(&mgr, &Id::try_from(id.as_str()).expect("an id"), RunInFlight::Leave);

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(calls_of(&fake), [read_call(&id)]);
}

/// A stand-in that stays, with the definition still held from goetia's path (the retry after a failed
/// reload of a `Leave` that wrote it): the reload is made and the stand-in and its run stay.
#[skuld::test]
fn a_retry_after_a_failed_reload_reloads_and_leaves_the_standin_and_its_run(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed_standin(&id, "15", "1min 30s");
    let fake = read_then_gate([state(&id, "activating", "start")], [reload()]);
    let mgr = over(&fake);

    let e = uninstall_absent(&mgr, &id, RunInFlight::Leave).expect_err("absent");

    assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
    assert_eq!(calls_of(&fake), [read_call(&id), reload_call()]);
    assert_eq!(standin_text(&id), Some(default_standin(&id)));
}

/// With `Stop` the stand-in's run is stopped as rule 2 says, and the stand-in and the held definition make one
/// reload.
#[skuld::test]
fn stopping_an_absent_ids_standin_whose_definition_is_also_held_makes_one_reload(
    #[fixture(systemd_group)] _group: &Group,
) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed_standin(&id, "15", "1min 30s");
    let fake = read_then_gate(
        [state(&id, "active", "running")],
        [enqueued("stop", 3), state(&id, "inactive", "dead"), reload()],
    );
    let mgr = over(&fake);

    let e = uninstall_absent(&mgr, &id, RunInFlight::Stop).expect_err("absent");

    assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
    assert_eq!(
        calls_of(&fake),
        [read_call(&id), stop_call(&id), read_call(&id), reload_call()]
    );
    assert_eq!(standin_text(&id), None);
}

/// A stand-in removed and a definition held make one reload.
#[skuld::test]
fn a_removed_standin_and_a_held_definition_make_one_reload(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed_standin(&id, "15", "1min 30s");
    let fake = read_then_gate([state(&id, "inactive", "dead")], [reload()]);
    let mgr = over(&fake);

    let e = uninstall_absent(&mgr, &id, RunInFlight::Leave).expect_err("absent");

    assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
    assert_eq!(calls_of(&fake), [read_call(&id), reload_call()]);
    assert_eq!(standin_text(&id), None);
}

/// A held definition that is a restart waiting is reloaded and no more: no read after it, no `reset-failed`,
/// since the files were not removed by this call.
#[skuld::test]
fn a_held_definition_whose_restart_is_waiting_is_only_reloaded(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    let fake = read_then_gate([state(&id, "activating", "auto-restart")], [reload()]);
    let mgr = over(&fake);

    let e = uninstall_absent(&mgr, &id, RunInFlight::Leave).expect_err("absent");

    assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
    assert_eq!(calls_of(&fake), [read_call(&id), reload_call()]);
}

/// A file at the stand-in's path that is not goetia's is not goetia's to stop or remove: the read is made, the
/// file is untouched.
#[skuld::test]
fn an_absent_ids_foreign_runtime_unit_is_left_alone(#[fixture(systemd_group)] _group: &Group) {
    let id = fresh_id("systemd");
    let _left = Leftovers::new(&id);
    seed_foreign_at(&id);

    for in_flight in [RunInFlight::Stop, RunInFlight::Leave] {
        let fake = read_then_gate([not_loaded()], []);
        let mgr = over(&fake);

        let e = uninstall_absent(&mgr, &id, in_flight).expect_err("absent");

        assert!(matches!(e, Error::NotInstalled { .. }), "{e:?}");
        assert_eq!(calls_of(&fake), [read_call(&id)], "{in_flight:?}: no stop, no removal");
        assert_eq!(standin_text(&id).as_deref(), Some(seed_foreign()));
    }
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
