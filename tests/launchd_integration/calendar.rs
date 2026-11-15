//! A calendar timer against what launchd itself registered for it. UserEventAgent logs, for every
//! `StartCalendarInterval` entry it arms, a `Registered` line with the entry's next fire, so the earliest of
//! them is the timer's next fire, and `next_run` (which goetia computes, as launchd reports none) must be it.
//!
//! The lines are read from a `log stream` that is attached before the timer is armed. Every wait here is on
//! the log's own lines, with no cap: the suite's watchdog is its bound.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use goetia::backend::launchd::manager::Launchd;
use goetia::manager::ServiceManager as _;
use goetia::testing::task;

use crate::launchd::{Guard, print_job};
use crate::support::cron_oracle::assert_next_run_strict;
use crate::support::tasks::{id_of, task_status};
use crate::support::{self, Group, cmd, launchd_group};
use crate::tasks::ChildGuard;

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The `Registered` lines of UserEventAgent, from a `log stream` that is attached when [`start`] returns.
///
/// [`start`]: Registrations::start
struct Registrations {
    lines: mpsc::Receiver<String>,
    _stream: ChildGuard,
}

impl Registrations {
    fn start() -> Self {
        // `2>&1`: the stream's first output, whichever of its two streams it goes to, shows that it runs.
        let mut stream = Command::new("/bin/sh")
            .args([
                "-c",
                "exec log stream --level debug --style compact --predicate 'eventMessage CONTAINS \"StartCalendarInterval\"' 2>&1",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn `log stream`");
        let stdout = stream.stdout.take().expect("a piped stdout");
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let registrations = Self {
            lines,
            _stream: ChildGuard(stream),
        };

        // The first line is the stream's own; a line logged after it reaches the stream.
        registrations.next_line();
        let canary = format!("StartCalendarInterval canary {}", std::process::id());
        cmd::run("logger", &[canary.as_str()]).expect_ok();
        while !registrations.next_line().contains(&canary) {}
        registrations
    }

    fn next_line(&self) -> String {
        self.lines.recv().expect("`log stream` ended")
    }

    /// The next fire of each of the `count` entries of the job `label`, one per `Registered` line.
    fn next_fires(&self, label: &str, count: usize) -> Vec<SystemTime> {
        let prefix = format!("Registered StartCalendarInterval: {label}.");
        let mut fires = BTreeMap::new();
        while fires.len() < count {
            let line = self.next_line();
            let Some((_, rest)) = line.split_once(prefix.as_str()) else {
                continue;
            };
            let (entry, date) = rest
                .split_once(": ")
                .unwrap_or_else(|| panic!("a `Registered` line with no date: {line}"));
            fires.insert(entry.to_string(), fire_of(date));
        }
        fires.into_values().collect()
    }
}

/// The instant of a date as UserEventAgent writes it, in the machine's zone:
/// `Monday, January 4, 2027 at 9:00:00 AM Greenwich Mean Time`, where a narrow no-break space (U+202F)
/// stands before `AM` or `PM`. The zone's name is not read: the machine's own rules turn the fields into an
/// instant.
fn fire_of(date: &str) -> SystemTime {
    let text = date.replace('\u{202f}', " ");
    let mut words = text.split_whitespace().skip(1);
    let (Some(month), Some(day), Some(year), Some("at"), Some(time), Some(half)) = (
        words.next(),
        words.next(),
        words.next(),
        words.next(),
        words.next(),
        words.next(),
    ) else {
        panic!("a date this test cannot read: {date:?}");
    };
    let number = |text: &str| -> i32 { text.parse().unwrap_or_else(|_| panic!("`{text}` in {date:?}")) };
    let [hour, minute, second] = {
        let mut parts = time.split(':').map(number);
        [(); 3].map(|()| parts.next().unwrap_or_else(|| panic!("a time in {date:?}")))
    };

    // SAFETY: an all-zero `tm` is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = number(year) - 1900;
    tm.tm_mon = MONTHS
        .iter()
        .position(|name| *name == month)
        .and_then(|month| i32::try_from(month).ok())
        .unwrap_or_else(|| panic!("a month in {date:?}"));
    tm.tm_mday = number(day.trim_end_matches(','));
    tm.tm_hour = hour % 12 + if half == "PM" { 12 } else { 0 };
    tm.tm_min = minute;
    tm.tm_sec = second;
    tm.tm_isdst = -1;
    // SAFETY: the pointer is valid for the call.
    let seconds = unsafe { libc::mktime(&mut tm) };
    UNIX_EPOCH + Duration::from_secs(u64::try_from(seconds).unwrap_or_else(|_| panic!("mktime failed for {date:?}")))
}

/// `next_run` of a timer with 180 entries (nine hours, four minutes, five weekdays) is the earliest of the
/// next fires launchd registered for them. The timer is armed after the stream is attached, and the lines
/// awaited are as many as `print` lists entries.
#[skuld::test]
fn next_run_is_the_earliest_fire_launchd_registered(#[fixture(launchd_group)] _group: &Group) {
    const SCHEDULE: &str = "*/15 9-17 * * 1-5";
    let id = support::random_test_id();
    let task_id = id_of(&id);
    let timer = format!("{id}@timer");
    let mgr = Launchd::new();
    let _guard = Guard::install(
        &mgr,
        &task(&id, ["/bin/sleep", "300"].map(String::from).to_vec(), Some(SCHEDULE)),
    );
    let registrations = Registrations::start();

    let before = SystemTime::now();
    mgr.enable(&task_id).expect("enable");
    let listed = print_job(&timer).stdout.matches("descriptor = ").count();
    assert_eq!(listed, 180, "`print` lists the entries of `{SCHEDULE}`");
    let fires = registrations.next_fires(&timer, listed);
    let earliest = *fires.iter().min().expect("a fire");
    let status = task_status(&mgr, &task_id);
    let after = SystemTime::now();

    // launchd armed the entries before `status` read the clock. When its earliest fire is still ahead at
    // the end, no fire time lay between the two reads, so goetia's answer is that fire. When it is not, a
    // fire time passed between them, and `next_run` is the next one after the one that passed.
    if earliest > after {
        assert_eq!(status.next_run, Some(earliest), "{status:?}");
    } else {
        assert_next_run_strict(&status, SCHEDULE, &task_id, before, after);
    }
}
