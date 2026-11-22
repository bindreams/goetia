//! The plan, cell by cell: every combination of (choice, call, stand-in file, run, held definition) a call can
//! reach is generated, and each must be covered by exactly one row of [`ROWS`]. A cell no row covers fails
//! the test, so a combination cannot be forgotten.

use super::*;
use crate::backend::systemd::manager::systemctl::RunPhase;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    /// `uninstall` of an installed id whose snapshot has the job's file.
    Job,
    /// `uninstall` of an installed id whose snapshot is a timer alone.
    NoJob,
    /// `uninstall` of an id nothing is installed at.
    Sweep,
    /// `install`, which settles a stand-in it finds as `Leave` does.
    Install,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum File {
    Absent,
    Ours,
    Foreign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    /// The call reads nothing.
    Unread,
    Idle,
    Failed,
    Pending,
    Start,
    Running,
}

const CHOICES: [RunInFlight; 2] = [RunInFlight::Stop, RunInFlight::Leave];
const CALLS: [Call; 4] = [Call::Job, Call::NoJob, Call::Sweep, Call::Install];
const FILES: [File; 3] = [File::Absent, File::Ours, File::Foreign];
const RUNS: [Run; 6] = [
    Run::Unread,
    Run::Idle,
    Run::Failed,
    Run::Pending,
    Run::Start,
    Run::Running,
];

#[derive(Debug, Clone, Copy)]
struct Cell {
    in_flight: RunInFlight,
    call: Call,
    file: File,
    run: Run,
    held_gone: bool,
}

impl Cell {
    fn holding(&self) -> Holding {
        match self.call {
            Call::Job => Holding::Job,
            Call::NoJob => Holding::NoJob,
            Call::Sweep | Call::Install => Holding::Absent,
        }
    }

    fn standin_file(&self) -> StandinFile {
        match self.file {
            File::Absent => StandinFile::Absent,
            File::Ours => StandinFile::Ours("text".to_string()),
            File::Foreign => StandinFile::NotOurs,
        }
    }

    fn installed(&self) -> bool {
        matches!(self.call, Call::Job | Call::NoJob)
    }

    /// The refusal of rule 5, as the table states it: only where a stand-in could be written.
    fn refused(&self) -> bool {
        self.in_flight == RunInFlight::Leave && self.call == Call::Job && self.file == File::Foreign
    }

    /// Whether a call of this shape reads the unit, as the table states it.
    fn reads(&self) -> bool {
        (self.in_flight == RunInFlight::Leave && self.installed())
            || self.file == File::Ours
            || self.call == Call::Sweep
    }

    /// A cell a call can be in: `install` settles as `Leave` does, a held definition is looked for only by
    /// `uninstall` of an absent id, and the run is read exactly when the call reads it.
    fn reachable(&self) -> bool {
        (self.call != Call::Install || self.in_flight == RunInFlight::Leave)
            && (!self.held_gone || (self.call == Call::Sweep && self.run != Run::Unread))
            && ((self.run != Run::Unread) == self.reads())
            && !self.refused()
    }
}

fn cells() -> Vec<Cell> {
    let mut all = Vec::new();
    for in_flight in CHOICES {
        for call in CALLS {
            for file in FILES {
                for run in RUNS {
                    for held_gone in [false, true] {
                        all.push(Cell {
                            in_flight,
                            call,
                            file,
                            run,
                            held_gone,
                        });
                    }
                }
            }
        }
    }
    all
}

fn unit_run(run: Run) -> UnitRun {
    UnitRun {
        phase: match run {
            Run::Unread | Run::Idle | Run::Failed => RunPhase::None,
            Run::Pending => RunPhase::PendingRestart,
            Run::Start => RunPhase::InStartJob,
            Run::Running => RunPhase::Running,
        },
        failed: run == Run::Failed,
        kill_signal: "12".to_string(),
        stop_timeout: "45s".to_string(),
        load_state: "loaded".to_string(),
        fragment_path: String::new(),
    }
}

fn plan_of(cell: &Cell) -> Plan {
    let file = cell.standin_file();
    let known = Known {
        in_flight: cell.in_flight,
        holding: cell.holding(),
        file: &file,
        asks_held: cell.call == Call::Sweep,
    };
    assert_eq!(known.reads(), cell.reads(), "{cell:?}: whether the unit is read");
    let seen = Seen {
        run: unit_run(cell.run),
        held_gone: cell.held_gone,
    };
    plan(&known, (cell.run != Run::Unread).then_some(&seen))
}

// The table ===========================================================================================================

struct Row {
    /// The rule the row shows.
    rule: &'static str,
    in_flight: &'static [RunInFlight],
    call: &'static [Call],
    file: &'static [File],
    run: &'static [Run],
    held_gone: &'static [bool],
    plan: fn() -> Plan,
}

impl Row {
    fn covers(&self, cell: &Cell) -> bool {
        self.in_flight.contains(&cell.in_flight)
            && self.call.contains(&cell.call)
            && self.file.contains(&cell.file)
            && self.run.contains(&cell.run)
            && self.held_gone.contains(&cell.held_gone)
    }
}

fn plan_with(stop: bool, standin: Standin, reload: bool) -> Plan {
    Plan { stop, standin, reload }
}

fn written() -> Standin {
    Standin::Write {
        kill_signal: "12".to_string(),
        stop_timeout: "45s".to_string(),
    }
}

use Call::{Install, Job, NoJob, Sweep};
use File::{Absent, Foreign, Ours};
use Run::{Failed, Idle, Pending, Running, Start, Unread};

const STOP: &[RunInFlight] = &[RunInFlight::Stop];
const LEAVE: &[RunInFlight] = &[RunInFlight::Leave];
const NOT_HELD: &[bool] = &[false];
const HELD: &[bool] = &[true];
const EITHER: &[bool] = &[false, true];
const READ: &[Run] = &[Idle, Failed, Pending, Start, Running];

const ROWS: &[Row] = &[
    // Stop ------------------------------------------------------------------------------------------------------------
    Row {
        rule: "2: Stop stops a held job; a file that is not ours at the stand-in's path changes nothing",
        in_flight: STOP,
        call: &[Job],
        file: &[Absent, Foreign],
        run: &[Unread],
        held_gone: NOT_HELD,
        plan: || plan_with(true, Standin::Nothing, true),
    },
    Row {
        rule: "2: Stop stops nothing when only a timer is held",
        in_flight: STOP,
        call: &[NoJob],
        file: &[Absent, Foreign],
        run: &[Unread],
        held_gone: NOT_HELD,
        plan: || plan_with(false, Standin::Nothing, true),
    },
    Row {
        rule: "2, 4: Stop with a stand-in of ours stops, then removes it and reloads; whether the run failed is read after the stop, whatever the read before it said",
        in_flight: STOP,
        call: &[Job, NoJob, Sweep],
        file: &[Ours],
        run: READ,
        held_gone: EITHER,
        plan: || {
            plan_with(
                true,
                Standin::Remove {
                    clearing: Clearing::ReadAfterStop,
                },
                true,
            )
        },
    },
    Row {
        rule: "7, 2: Stop of an id nothing is installed at, with no stand-in of ours, stops and changes nothing",
        in_flight: STOP,
        call: &[Sweep],
        file: &[Absent, Foreign],
        run: READ,
        held_gone: NOT_HELD,
        plan: || plan_with(false, Standin::Nothing, false),
    },
    Row {
        rule: "7: a definition held for a file that is gone is reloaded and nothing else is done for it",
        in_flight: STOP,
        call: &[Sweep],
        file: &[Absent, Foreign],
        run: READ,
        held_gone: HELD,
        plan: || plan_with(false, Standin::Nothing, true),
    },
    // Leave -----------------------------------------------------------------------------------------------------------
    Row {
        rule: "1, 3: Leave of a job in its start job writes the stand-in (replacing one of ours) and stops nothing",
        in_flight: LEAVE,
        call: &[Job],
        file: &[Absent, Ours],
        run: &[Start],
        held_gone: NOT_HELD,
        plan: || plan_with(false, written(), true),
    },
    Row {
        rule: "1, 3: Leave of a job with no process, past its start job, or with a restart waiting (which gets no stand-in either) removes the files and sends nothing",
        in_flight: LEAVE,
        call: &[Job],
        file: &[Absent],
        run: &[Idle, Failed, Pending, Running],
        held_gone: NOT_HELD,
        plan: || plan_with(false, Standin::Nothing, true),
    },
    Row {
        rule: "3, 5: no stand-in for a timer alone, whatever its unit is doing; a file that is not ours at the path is no concern of it",
        in_flight: LEAVE,
        call: &[NoJob],
        file: &[Absent, Foreign],
        run: READ,
        held_gone: NOT_HELD,
        plan: || plan_with(false, Standin::Nothing, true),
    },
    Row {
        rule: "4: Leave removes a stand-in of ours whose run has no process, a restart that is waiting included",
        in_flight: LEAVE,
        call: &[Job, NoJob],
        file: &[Ours],
        run: &[Idle, Pending],
        held_gone: NOT_HELD,
        plan: || plan_with(false, Standin::Remove { clearing: Clearing::Not }, true),
    },
    Row {
        rule: "4: and one whose run failed, after reset-failed",
        in_flight: LEAVE,
        call: &[Job, NoJob],
        file: &[Ours],
        run: &[Failed],
        held_gone: NOT_HELD,
        plan: || {
            plan_with(
                false,
                Standin::Remove {
                    clearing: Clearing::Needed,
                },
                true,
            )
        },
    },
    Row {
        rule: "4: the stand-in stays while the run has a process",
        in_flight: LEAVE,
        call: &[Job],
        file: &[Ours],
        run: &[Running],
        held_gone: NOT_HELD,
        plan: || plan_with(false, Standin::Nothing, true),
    },
    Row {
        rule: "3, 4: a timer alone gets no stand-in, and one of ours at its id stays while the run has a process",
        in_flight: LEAVE,
        call: &[NoJob],
        file: &[Ours],
        run: &[Start, Running],
        held_gone: NOT_HELD,
        plan: || plan_with(false, Standin::Nothing, true),
    },
    Row {
        rule: "7, 3: Leave of an id nothing is installed at changes nothing: no stop, no stand-in",
        in_flight: LEAVE,
        call: &[Sweep],
        file: &[Absent, Foreign],
        run: READ,
        held_gone: NOT_HELD,
        plan: || plan_with(false, Standin::Nothing, false),
    },
    Row {
        rule: "7: and reloads a definition held for a file that is gone, with no read after it",
        in_flight: LEAVE,
        call: &[Sweep],
        file: &[Absent, Foreign],
        run: READ,
        held_gone: HELD,
        plan: || plan_with(false, Standin::Nothing, true),
    },
    Row {
        rule: "4, 7: Leave removes a stand-in of ours with no process, and reloads",
        in_flight: LEAVE,
        call: &[Sweep],
        file: &[Ours],
        run: &[Idle, Pending],
        held_gone: EITHER,
        plan: || plan_with(false, Standin::Remove { clearing: Clearing::Not }, true),
    },
    Row {
        rule: "4, 7: and one whose run failed, after reset-failed",
        in_flight: LEAVE,
        call: &[Sweep],
        file: &[Ours],
        run: &[Failed],
        held_gone: EITHER,
        plan: || {
            plan_with(
                false,
                Standin::Remove {
                    clearing: Clearing::Needed,
                },
                true,
            )
        },
    },
    Row {
        rule: "4, 7: the stand-in and its run stay; the reload only for a held definition",
        in_flight: LEAVE,
        call: &[Sweep],
        file: &[Ours],
        run: &[Start, Running],
        held_gone: NOT_HELD,
        plan: || plan_with(false, Standin::Nothing, false),
    },
    Row {
        rule: "7: the stand-in and its run stay and a held definition is reloaded",
        in_flight: LEAVE,
        call: &[Sweep],
        file: &[Ours],
        run: &[Start, Running],
        held_gone: HELD,
        plan: || plan_with(false, Standin::Nothing, true),
    },
    // install ---------------------------------------------------------------------------------------------------------
    Row {
        rule: "8: an install with no stand-in of ours reads nothing and does nothing",
        in_flight: LEAVE,
        call: &[Install],
        file: &[Absent, Foreign],
        run: &[Unread],
        held_gone: NOT_HELD,
        plan: || plan_with(false, Standin::Nothing, false),
    },
    Row {
        rule: "8, 4: an install removes a stand-in of ours with no process, and reloads",
        in_flight: LEAVE,
        call: &[Install],
        file: &[Ours],
        run: &[Idle, Pending],
        held_gone: NOT_HELD,
        plan: || plan_with(false, Standin::Remove { clearing: Clearing::Not }, true),
    },
    Row {
        rule: "8, 4: and one whose run failed, after reset-failed",
        in_flight: LEAVE,
        call: &[Install],
        file: &[Ours],
        run: &[Failed],
        held_gone: NOT_HELD,
        plan: || {
            plan_with(
                false,
                Standin::Remove {
                    clearing: Clearing::Needed,
                },
                true,
            )
        },
    },
    Row {
        rule: "8, 4: an install leaves a stand-in whose run has a process, and never stops it",
        in_flight: LEAVE,
        call: &[Install],
        file: &[Ours],
        run: &[Start, Running],
        held_gone: NOT_HELD,
        plan: || plan_with(false, Standin::Nothing, false),
    },
];

/// Every cell a call can be in has exactly one row, and the plan is the row's.
#[skuld::test]
fn every_reachable_cell_has_one_row_and_gets_its_plan() {
    let mut covered = vec![0usize; ROWS.len()];
    let mut reachable = 0;
    for cell in cells().into_iter().filter(Cell::reachable) {
        reachable += 1;
        let rows: Vec<usize> = (0..ROWS.len()).filter(|&at| ROWS[at].covers(&cell)).collect();
        assert_eq!(
            rows.len(),
            1,
            "{cell:?} is covered by {} rows: {:?}",
            rows.len(),
            rows.iter().map(|&at| ROWS[at].rule).collect::<Vec<_>>()
        );
        covered[rows[0]] += 1;
        let row = &ROWS[rows[0]];
        assert_eq!(plan_of(&cell), (row.plan)(), "{cell:?}\nrule {}", row.rule);
    }
    for (row, count) in ROWS.iter().zip(&covered) {
        assert!(
            *count > 0,
            "no reachable cell is covered by the row for rule {}",
            row.rule
        );
    }
    assert!(reachable > 100, "the generator reached {reachable} cells");
}

/// Rule 5: only `Leave` over an id whose job goetia removes is refused, and only for a file that is not ours:
/// a timer alone never gets a stand-in, so a file at the path is no concern of it.
#[skuld::test]
fn a_file_that_is_not_ours_refuses_leave_on_a_held_job_and_nothing_else() {
    for cell in cells() {
        let file = cell.standin_file();
        let known = Known {
            in_flight: cell.in_flight,
            holding: cell.holding(),
            file: &file,
            asks_held: cell.call == Call::Sweep,
        };
        assert_eq!(known.blocked(), cell.refused(), "{cell:?}");
    }
}

/// The values a stand-in carries are the ones the read printed, for any `Leave` of a job in its start job.
#[skuld::test]
fn the_stand_in_carries_the_values_the_read_printed() {
    let mut run = unit_run(Start);
    run.kill_signal = "2".to_string();
    run.stop_timeout = "infinity".to_string();
    let file = StandinFile::Absent;
    let known = Known {
        in_flight: RunInFlight::Leave,
        holding: Holding::Job,
        file: &file,
        asks_held: false,
    };

    let planned = plan(
        &known,
        Some(&Seen {
            run,
            held_gone: false,
        }),
    );

    assert_eq!(
        planned.standin,
        Standin::Write {
            kill_signal: "2".to_string(),
            stop_timeout: "infinity".to_string()
        }
    );
}
