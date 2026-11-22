//! What the tests of a task's verbs share: the fixtures of the one task `nightly`, its record
//! directory, and the machine's processes as a table the test states.

use std::io;
use std::path::PathBuf;

use tempfile::TempDir;
use windows::Win32::System::Time::TIME_ZONE_INFORMATION;

use super::TaskSnapshot;
use super::api::Instance;
use super::fake::Fake;
use super::live::Liveness;
use super::verbs::Processes;
use crate::backend::kill::Waited;
use crate::backend::scm::instance::InstanceId;
use crate::backend::scm::manager::Scm;
use crate::backend::scm::runs::{RunOutcome, RunRecord, Runs};
use crate::backend::scm::shim::acl::Descriptor;
use crate::backend::scm::sid::try_sid_display;
use crate::backend::scm::test_support::registered_task;
use crate::blob::Blob;
use crate::elevation::test_support::process_user_sid;
use crate::manager::budget::Deadline;
use crate::spec::{DaemonSpec, Id};

/// A zone with no change of clocks, `utc_offset_hours` east of UTC: the offset of a conversion that a
/// test pins, where the machine's own zone is whatever the runner has.
pub(super) fn fixed_zone(utc_offset_hours: i32) -> TIME_ZONE_INFORMATION {
    TIME_ZONE_INFORMATION {
        // `Bias` is the minutes to add to local time to get UTC: west of it is positive.
        Bias: -utc_offset_hours * 60,
        ..TIME_ZONE_INFORMATION::default()
    }
}

pub(super) fn id() -> Id {
    Id::try_from("nightly").unwrap()
}

pub(super) const PATH: &str = r"\Goetia\nightly";

pub(super) fn instance_id(n: u8) -> InstanceId {
    InstanceId::parse(&format!("{n:032x}")).unwrap()
}

pub(super) fn instance(n: u8, pid: u32) -> Instance {
    Instance {
        id: instance_id(n),
        pid,
    }
}

/// A manager whose Task Scheduler is `fake`.
pub(super) fn scm(fake: &Fake) -> Scm {
    Scm::new().with_tasks(Box::new(fake.clone()))
}

pub(super) fn snapshot_of(spec: &DaemonSpec, armed: bool) -> TaskSnapshot {
    let blob = Blob {
        schema: crate::blob::SCHEMA,
        version: crate::version().to_string(),
        spec: spec.clone(),
    };
    TaskSnapshot::new(id(), Ok(blob), registered_task(spec, armed))
}

/// The task's record directory in a temporary `%ProgramData%`, with the permissions an install gives it
/// for the account this test runs as (the test removes it). The folders are made plainly first: naming
/// Administrators as their owner, as an install does, needs an elevated token, and an existing directory
/// has only its permissions set.
pub(super) fn records() -> (TempDir, PathBuf, Runs) {
    let dir = tempfile::tempdir().unwrap();
    let runs = Runs::of(dir.path(), &id());
    let path = dir.path().join("Goetia").join("runs").join("nightly");
    std::fs::create_dir_all(&path).unwrap();
    runs.ensure(&[process_user_sid()]).unwrap();
    (dir, path, runs)
}

/// The record directory with no right for the account this test runs as but to read and change its own
/// permissions, until it is dropped: nothing can be created in it, as for a caller who is not an
/// Administrator. Dropping restores the entry the directory is made with, so the test's directory can
/// be removed.
pub(super) struct Denied<'a>(&'a std::path::Path);

impl<'a> Denied<'a> {
    pub(super) fn new(path: &'a std::path::Path) -> Self {
        let me = try_sid_display(&process_user_sid()).unwrap();
        // READ_CONTROL, WRITE_DAC and SYNCHRONIZE (which `CreateFile` adds to every open).
        let denied = Descriptor::from_sddl(&format!("D:P(A;;FA;;;SY)(A;;0x160000;;;{me})")).unwrap();
        crate::backend::scm::test_support::set_dacl(&denied, path).unwrap();
        Denied(path)
    }
}

impl Drop for Denied<'_> {
    fn drop(&mut self) {
        let restored = Descriptor::for_runs(&[process_user_sid()])
            .map(|wanted| crate::backend::scm::test_support::set_dacl(&wanted, self.0));
        if !matches!(restored, Ok(Ok(()))) {
            eprintln!(
                "warning: could not restore the permissions of {}: {restored:?}",
                self.0.display()
            );
        }
    }
}

/// How many pins the directory `path` holds.
pub(super) fn pins_in(path: &std::path::Path) -> usize {
    std::fs::read_dir(path)
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().file_name().to_string_lossy().ends_with(".pin"))
        .count()
}

/// [`records`] where the task's directory is a junction, as its account can make it (adding a file is all
/// that takes). The target holds an incomplete stop of the run `1`, which a read through the link would
/// find: nothing may. The last is the target, which outlasts the link.
pub(super) fn linked_records() -> (TempDir, PathBuf, Runs, TempDir) {
    let (dir, path, runs) = records();
    let target = tempfile::tempdir().unwrap();
    std::fs::write(
        target.path().join(format!("{}.json", instance_id(1).hex())),
        br#"{"started":1,"outcome":"incomplete_stop"}"#,
    )
    .unwrap();
    std::fs::remove_dir(&path).unwrap();
    crate::backend::scm::shim::acl::acl_tests::windows::junction(&path, target.path());
    (dir, path, runs, target)
}

/// What a shim does as its run ends: it writes the run's record, and its prune has nothing to say.
pub(super) fn shim_writes(shim: &Runs, record: &RunRecord) {
    assert_eq!(shim.write(record, None).unwrap(), Vec::<String>::new());
}

/// What a shim does as it hears a stop with its program alive: it writes the provisional file of the run `n`'s first
/// `Stopped`, under its own pin ([`Pin::write_stopping`]).
pub(super) fn shim_writes_stopping(shim: &Runs, n: u8) {
    let pin = shim.hold().unwrap();
    pin.write_stopping(&instance_id(n), 1).unwrap();
}

pub(super) fn record(n: u8, outcome: RunOutcome) -> RunRecord {
    RunRecord {
        instance: instance_id(n),
        started: 1,
        outcome,
    }
}

/// The machine's processes, as the test says: `alive` pids run, `holdable` ones can be opened,
/// `denied` ones are refused, and waiting for a `lingering` one runs out its deadline. Waiting for any
/// other runs `on_exit` (what the shim does as it ends). Every call notes how many calls the
/// scheduler had had by then.
pub(super) struct Machine {
    fake: Fake,
    pub alive: Vec<u32>,
    pub holdable: Vec<u32>,
    pub denied: Vec<u32>,
    pub lingering: Vec<u32>,
    pub held: Vec<u32>,
    pub terminated: Vec<(u32, usize)>,
    pub waited: Vec<(u32, usize)>,
    pub on_exit: Box<dyn FnMut(u32) + Send>,
}

impl Machine {
    /// Every pid in `alive` runs and can be held.
    pub(super) fn new(fake: &Fake, alive: &[u32]) -> Self {
        Self {
            fake: fake.clone(),
            alive: alive.to_vec(),
            holdable: alive.to_vec(),
            denied: Vec::new(),
            lingering: Vec::new(),
            held: Vec::new(),
            terminated: Vec::new(),
            waited: Vec::new(),
            on_exit: Box::new(|_| {}),
        }
    }
}

impl Processes for Machine {
    type Held = u32;

    fn alive(&mut self, pid: u32) -> io::Result<Liveness> {
        Ok(if self.alive.contains(&pid) {
            Liveness::Alive
        } else {
            Liveness::Gone
        })
    }

    fn open(&mut self, pid: u32) -> io::Result<Option<u32>> {
        self.held.push(pid);
        if self.denied.contains(&pid) {
            return Err(io::Error::from_raw_os_error(5));
        }
        Ok(self.holdable.contains(&pid).then_some(pid))
    }

    fn force_end(&mut self, held: &u32) -> io::Result<()> {
        self.terminated.push((*held, self.fake.calls().len()));
        Ok(())
    }

    fn await_exit(&mut self, held: &u32, _deadline: Deadline) -> io::Result<Waited> {
        self.waited.push((*held, self.fake.calls().len()));
        if self.lingering.contains(held) {
            return Ok(Waited::Expired);
        }
        (self.on_exit)(*held);
        Ok(Waited::Confirmed)
    }
}
