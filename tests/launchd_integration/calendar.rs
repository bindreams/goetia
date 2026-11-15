//! A calendar timer against what launchd itself registered for it. UserEventAgent logs, for every
//! `StartCalendarInterval` entry it arms, a `Registered` line with the entry's next fire, so the earliest of
//! them is the timer's next fire, and `next_run` (which goetia computes, as launchd reports none) must be it.
//!
//! The lines are read from a `log stream` that is attached before the timer is armed. Every wait here is on
//! the log's own lines, with no cap: the suite's watchdog is its bound. (The canary is logged again while it
//! has not come back; see `Registrations::start`.)

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use goetia::backend::launchd::manager::Launchd;
use goetia::manager::ServiceManager as _;
use goetia::testing::{system_zone, task};
use jiff::civil::DateTime;
use jiff::tz::TimeZone;

use crate::launchd::{Guard, print_job};
use crate::support::cron_oracle::assert_next_run_strict;
use crate::support::tasks::{id_of, task_status};
use crate::support::{self, Group, cmd, launchd_group};
use crate::tasks::ChildGuard;

/// How long the canary waits for a line before another is logged. Not a measure of anything: a canary that
/// is not lost is not logged twice, and one logged twice does no harm.
const RELOG: Duration = Duration::from_millis(100);

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

        // The first line is the stream's own, printed before the stream is subscribed to the log (measured: a
        // line logged straight after it was lost). The second, a column header, is printed with the first
        // event the stream delivers, and with none to deliver (the canary was lost) it never comes: in the
        // measured run it came 55 s later, with an unrelated event. So the canary is logged again once `RELOG`
        // has passed since the last one, whether or not other lines arrive meanwhile. That spacing proves
        // nothing about the stream: the wait ends only on the canary coming back. A `logger` that fails, or a
        // stream that ends, ends the test.
        registrations.next_line();
        let canary = format!("StartCalendarInterval canary {}", std::process::id());
        let log_canary = || {
            cmd::run("logger", &[canary.as_str()]).expect_ok();
            std::time::Instant::now()
        };
        let mut logged = log_canary();
        loop {
            match registrations.lines.recv_timeout(RELOG.saturating_sub(logged.elapsed())) {
                Ok(line) if line.contains(&canary) => return registrations,
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {
                    if logged.elapsed() >= RELOG {
                        logged = log_canary();
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("`log stream` ended"),
            }
        }
    }

    fn next_line(&self) -> String {
        self.lines.recv().expect("`log stream` ended")
    }

    /// The next fire of each of the `count` entries of the job `label`, one per `Registered` line, read in
    /// `zone` and not before `after`.
    fn next_fires(&self, label: &str, count: usize, zone: &TimeZone, after: SystemTime) -> Vec<SystemTime> {
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
            fires.insert(entry.to_string(), fire_of(date, zone, after));
        }
        fires.into_values().collect()
    }
}

/// The instant of a date as UserEventAgent writes it:
/// `Monday, January 4, 2027 at 9:00:00 AM Greenwich Mean Time`, where a narrow no-break space (U+202F)
/// stands before `AM` or `PM`. The zone's name is not read: `zone`, the machine's zone as goetia reads it,
/// turns the fields into an instant.
///
/// A time the zone repeats (the clocks go back) is two instants, and the text does not say which. The fire is
/// the first of them that is after `after`, the clock read before launchd was told to register: launchd
/// registers the first instant after its own clock.
fn fire_of(date: &str, zone: &TimeZone, after: SystemTime) -> SystemTime {
    let text = date.replace('\u{202f}', " ");
    // The words up to `AM` or `PM`; what follows is the zone's name.
    let words: Vec<&str> = text.split_whitespace().collect();
    let end = words
        .iter()
        .position(|word| matches!(*word, "AM" | "PM"))
        .unwrap_or_else(|| panic!("a date with no AM or PM: {date:?}"));
    let local = words[..=end].join(" ");
    let civil = DateTime::strptime("%A, %B %d, %Y at %I:%M:%S %p", &local)
        .unwrap_or_else(|e| panic!("a date this test cannot read: {date:?}: {e}"));

    let ambiguous = zone.to_ambiguous_timestamp(civil);
    [ambiguous.earlier(), ambiguous.later()]
        .into_iter()
        .map(|instant| SystemTime::from(instant.unwrap_or_else(|e| panic!("{date:?} is not an instant: {e}"))))
        .find(|&instant| instant > after)
        .unwrap_or_else(|| panic!("{date:?} is not after the clock read before launchd registered it"))
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
    let zone = system_zone().expect("the system time zone");

    let before = SystemTime::now();
    mgr.enable(&task_id).expect("enable");
    let listed = print_job(&timer).stdout.matches("descriptor = ").count();
    assert_eq!(listed, 180, "`print` lists the entries of `{SCHEDULE}`");
    let fires = registrations.next_fires(&timer, listed, &zone, before);
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
