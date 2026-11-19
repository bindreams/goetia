//! Reading a task's XML as Task Scheduler stores it: the definition ([`TaskXml::parse`]), the armed
//! state, which is whether it has triggers ([`TaskXml::armed`]), and the two edits of the stored XML
//! that arm and disarm it ([`TaskXml::with_triggers`], [`TaskXml::without_triggers`]).

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::PathBuf;

use cosca::quote::windows::split_wide;
use roxmltree::{Document, Node};

use super::{
    MONTHS, NEVER_AT, SETTINGS, START_DATE, Scope, TASK_MODE_ARG, TaskXml, WEEKDAYS, account_of, extract_from_data,
    triggers_of, triggers_xml,
};
use crate::backend::scm::instance::TaskAccount;
use crate::backend::scm::triggers::{Repeat, Trigger};
use crate::error::Error;
use crate::manager::flow::NOTHING_TO_ARM;
use crate::shim::HostCommand;
use crate::spec::{DaemonSpec, Id, invalid};

impl TaskXml {
    /// The definition of the installed task whose XML is `xml` and whose `Data` is `data`
    /// (`ITaskDefinition.Data`, LF only; the CRLF of the `Xml` form reads the same).
    ///
    /// The blob decides the id, the name and the account. The principal is not read: Task Scheduler
    /// rewrites a name to a SID. The settings are read, an element Task Scheduler dropped as its
    /// default read as that default, so a hand edit of one shows as a difference. A trigger or an
    /// action goetia does not write is kept as text in the definition, so that it shows as a
    /// difference too. A task with no trigger is a disarmed one, and its triggers are the ones its
    /// blob's schedule compiles to. An element goetia does not own (`URI`, `UseUnifiedSchedulingEngine`,
    /// ...) is ignored.
    ///
    /// An error: `data` is not a task's blob, the XML is not well-formed, or it has no `Exec` action.
    pub(crate) fn parse(xml: &str, data: &str) -> Result<TaskXml, Error> {
        let data = data.trim().replace("\r\n", "\n");
        let Some(blob) = extract_from_data(&data)? else {
            return Err(Error::blob("the task's `Data` does not hold goetia's metadata"));
        };
        let DaemonSpec::Task(_) = &blob.spec else {
            return Err(Error::blob("the task's metadata is a service's, not a task's"));
        };
        let common = blob.spec.common();
        let account = account_of(&common.id, &common.user)?;

        let document = document(xml)?;
        let root = document.root_element();
        let settings = SETTINGS
            .iter()
            .map(|setting| {
                let scope = match setting.scope {
                    Scope::Principal => child(root, "Principals").and_then(|node| child(node, "Principal")),
                    Scope::Settings => child(root, "Settings"),
                    Scope::Idle => child(root, "Settings").and_then(|node| child(node, "IdleSettings")),
                };
                scope
                    .and_then(|node| child(node, setting.name))
                    .map_or_else(|| setting.absent.to_owned(), text)
            })
            .collect();

        let mut unrecognised = Vec::new();
        let stored: Vec<Node<'_, '_>> = child(root, "Triggers").into_iter().flat_map(elements).collect();
        let triggers = if stored.is_empty() {
            // A disarmed task has none: it reads as what arming it would write.
            triggers_of(&blob.spec)?
        } else {
            let mut read = Vec::new();
            for node in stored {
                match trigger_of(node) {
                    Some(trigger) => read.push(trigger),
                    None => unrecognised.push(squashed(node)),
                }
            }
            read
        };
        let host = action_of(root, &common.id, &mut unrecognised)?;

        Ok(TaskXml {
            id: common.id.clone(),
            name: common.name.clone(),
            host,
            account,
            triggers,
            data,
            settings,
            unrecognised,
        })
    }

    /// The account `xml`'s principal names, for registering the task again when its blob cannot say
    /// (see [`TaskXml::parse`], which reads it from the blob): a password logon is a user's, any other
    /// a service account's. The name is as Task Scheduler stores it, usually a SID.
    pub(crate) fn principal(xml: &str) -> Result<TaskAccount, Error> {
        let document = document(xml)?;
        let principal = child(document.root_element(), "Principals")
            .and_then(|principals| child(principals, "Principal"))
            .and_then(|principal| child(principal, "UserId").map(|user| (principal, text(user))))
            .filter(|(_, user)| !user.is_empty());
        let Some((principal, user)) = principal else {
            return Err(Error::blob("the task's XML names no principal"));
        };
        let password = child(principal, "LogonType").is_some_and(|logon| text(logon).eq_ignore_ascii_case("Password"));
        Ok(if password {
            TaskAccount::User(user)
        } else {
            TaskAccount::Service(user)
        })
    }

    /// The program of every `Exec` action of `xml`, as written. Read from the XML alone, so it names
    /// what any task in the folder runs, goetia's or not.
    pub(crate) fn executables(xml: &str) -> Result<Vec<String>, Error> {
        let document = document(xml)?;
        let Some(actions) = child(document.root_element(), "Actions") else {
            return Ok(Vec::new());
        };
        Ok(elements(actions)
            .filter(|action| action.tag_name().name() == "Exec")
            .filter_map(|exec| child(exec, "Command").map(text))
            .collect())
    }

    /// Whether the task `xml` describes has a trigger and is itself enabled (`Settings/Enabled`,
    /// which Task Scheduler leaves out while it is true): a task that is not enabled is started by
    /// nothing, and one with no trigger has nothing to start it. XML that cannot be read is not armed.
    pub(crate) fn armed(xml: &str) -> bool {
        let Ok(document) = document(xml) else {
            return false;
        };
        own_flag_is_true(document.root_element())
            && child(document.root_element(), "Triggers").is_some_and(|triggers| elements(triggers).next().is_some())
    }

    /// `xml` with its triggers taken out (the `Triggers` element, empty or not, a hand-added trigger
    /// among them) and nothing else changed but the task's own flag: a hand edit elsewhere in the
    /// task is kept. This is what `disable` registers; the next `enable` writes the spec's triggers.
    ///
    /// **The task's own flag (`Settings/Enabled`) is set true,** where it is stated: an `uninstall`
    /// that did not finish leaves it false, and `enable` and `disable` both make the task one that can
    /// be started again. One that states none is enabled already.
    pub(crate) fn without_triggers(xml: &str) -> Result<String, Error> {
        let document = document(xml)?;
        let mut edits = own_flag_edit(&document);
        if let Some(triggers) = child(document.root_element(), "Triggers") {
            edits.push((triggers.range(), String::new()));
        }
        Ok(edited(xml, edits))
    }

    /// `xml` with the triggers `spec`'s schedule compiles to in place of any it holds (a hand-added
    /// trigger is replaced with the rest), before `Actions` when it holds none, and nothing else
    /// changed but the task's own flag, which is set true as [`TaskXml::without_triggers`] sets it.
    /// This is what `enable` registers: the blob names the schedule, so the triggers need not be in
    /// the stored XML. A spec with no schedule has nothing to arm: [`Error::Invalid`].
    pub(crate) fn with_triggers(xml: &str, spec: &DaemonSpec) -> Result<String, Error> {
        let triggers = triggers_of(spec)?;
        if triggers.is_empty() {
            return Err(invalid(&spec.common().id, NOTHING_TO_ARM));
        }
        let text = triggers_xml(&triggers);
        let document = document(xml)?;
        let mut edits = own_flag_edit(&document);
        edits.push(match child(document.root_element(), "Triggers") {
            Some(stored) => (stored.range(), text),
            None => {
                let actions =
                    child(document.root_element(), "Actions").ok_or_else(|| Error::blob("the task has no actions"))?;
                let at = actions.range().start;
                (at..at, text)
            }
        });
        Ok(edited(xml, edits))
    }
}

/// The edit that sets the task's own flag true, where the XML states it.
fn own_flag_edit(document: &Document<'_>) -> Vec<(Range<usize>, String)> {
    child(document.root_element(), "Settings")
        .and_then(|settings| child(settings, "Enabled"))
        .map(|flag| match flag.first_child().filter(Node::is_text) {
            Some(value) => (value.range(), "true".to_owned()),
            None => (flag.range(), "<Enabled>true</Enabled>".to_owned()),
        })
        .into_iter()
        .collect()
}

/// `xml` with `edits` made, each a range of `xml` and its replacement.
fn edited(xml: &str, mut edits: Vec<(Range<usize>, String)>) -> String {
    // From the end, so that an edit does not move the ones before it.
    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    let mut out = xml.to_owned();
    for (range, text) in edits {
        out.replace_range(range, &text);
    }
    out
}

// The document --------------------------------------------------------------------------------------------------------

fn document(xml: &str) -> Result<Document<'_>, Error> {
    Document::parse(xml).map_err(|source| Error::blob_from("the task's XML cannot be read", source))
}

/// The child element of `node` named `name`, whatever its namespace.
fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    elements(node).find(|child| child.tag_name().name() == name)
}

fn elements<'a, 'i>(node: Node<'a, 'i>) -> impl Iterator<Item = Node<'a, 'i>> {
    node.children().filter(Node::is_element)
}

/// The text of `node`, without its outer whitespace.
fn text(node: Node<'_, '_>) -> String {
    node.text().unwrap_or_default().trim().to_owned()
}

/// `node` as it was written, its whitespace collapsed: what is kept of what goetia does not read.
fn squashed(node: Node<'_, '_>) -> String {
    node.document().input_text()[node.range()]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether every element child of `node` is named in `allowed`.
fn only(node: Node<'_, '_>, allowed: &[&str]) -> Option<()> {
    elements(node)
        .all(|child| allowed.contains(&child.tag_name().name()))
        .then_some(())
}

/// Whether the task's own flag, `Settings/Enabled`, is true: it is when Task Scheduler states none.
fn own_flag_is_true(root: Node<'_, '_>) -> bool {
    flag_is_true(child(root, "Settings").and_then(|settings| child(settings, "Enabled")))
}

/// An `Enabled` element is true unless it says false; a missing one is Task Scheduler's default, true.
fn flag_is_true(flag: Option<Node<'_, '_>>) -> bool {
    flag.is_none_or(|flag| !matches!(text(flag).to_ascii_lowercase().as_str(), "false" | "0"))
}

// The action ----------------------------------------------------------------------------------------------------------

/// The host the task's action runs: the program, and the arguments before the task-mode ending. An
/// action that does not end so keeps all its arguments and is recorded in `unrecognised`, as is
/// anything else the actions hold.
fn action_of(root: Node<'_, '_>, id: &Id, unrecognised: &mut Vec<String>) -> Result<HostCommand, Error> {
    let actions = child(root, "Actions").ok_or_else(|| Error::blob("the task has no actions"))?;
    let mut actions = elements(actions);
    let exec = actions
        .next()
        .filter(|node| node.tag_name().name() == "Exec")
        .ok_or_else(|| Error::blob("the task's first action is not an `Exec`"))?;
    unrecognised.extend(actions.map(squashed));
    let command = child(exec, "Command").ok_or_else(|| Error::blob("the task's action has no `Command`"))?;
    for node in elements(exec) {
        if !["Command", "Arguments"].contains(&node.tag_name().name()) {
            unrecognised.push(squashed(node));
        }
    }

    let mut args = split_arguments(&child(exec, "Arguments").map(text).unwrap_or_default());
    if args.ends_with(&[TASK_MODE_ARG.to_owned(), id.as_str().to_owned()]) {
        args.truncate(args.len() - 2);
    } else {
        unrecognised.push(format!("Arguments do not end with {TASK_MODE_ARG} {id}"));
    }
    Ok(HostCommand {
        exe: PathBuf::from(text(command)),
        args,
    })
}

/// The arguments of an `Arguments` text, as `CommandLineToArgvW` reads them after the program.
fn split_arguments(arguments: &str) -> Vec<String> {
    let line: Vec<u16> = format!("x {arguments}").encode_utf16().collect();
    split_wide(&line)
        .expect("splitting a Windows command line has no error state")
        .into_iter()
        .skip(1)
        .map(|arg| String::from_utf16_lossy(&arg))
        .collect()
}

// The triggers --------------------------------------------------------------------------------------------------------

/// The [`Trigger`] goetia writes that `node` is, or `None` for any other trigger.
fn trigger_of(node: Node<'_, '_>) -> Option<Trigger> {
    match node.tag_name().name() {
        "TimeTrigger" => {
            only(node, &["StartBoundary", "Enabled"])?;
            (text(child(node, "StartBoundary")?) == NEVER_AT).then_some(Trigger::Never)
        }
        "CalendarTrigger" => {
            let schedules = [
                "ScheduleByDay",
                "ScheduleByWeek",
                "ScheduleByMonth",
                "ScheduleByMonthDayOfWeek",
            ];
            only(
                node,
                &[&["StartBoundary", "Enabled", "Repetition"][..], &schedules].concat(),
            )?;
            let at = at_of(&text(child(node, "StartBoundary")?))?;
            let repeat = match child(node, "Repetition") {
                Some(repetition) => Some(repeat_of(repetition)?),
                None => None,
            };
            let mut found = elements(node).filter(|child| schedules.contains(&child.tag_name().name()));
            let schedule = found.next()?;
            found.next().is_none().then_some(())?;
            schedule_of(schedule, at, repeat)
        }
        _ => None,
    }
}

/// The time of day of a start boundary on [`START_DATE`], local, with no seconds and no zone.
fn at_of(boundary: &str) -> Option<(u8, u8)> {
    let time = boundary.strip_prefix(START_DATE)?.strip_prefix('T')?;
    let (hour, rest) = time.split_once(':')?;
    let (minute, seconds) = rest.split_once(':')?;
    let (hour, minute): (u8, u8) = (hour.parse().ok()?, minute.parse().ok()?);
    (seconds == "00" && hour < 24 && minute < 60 && time.len() == 8).then_some((hour, minute))
}

fn repeat_of(node: Node<'_, '_>) -> Option<Repeat> {
    only(node, &["Interval", "Duration", "StopAtDurationEnd"])?;
    if child(node, "StopAtDurationEnd").is_some_and(|stop| !text(stop).eq_ignore_ascii_case("false")) {
        return None;
    }
    let every_minutes = minutes_of(&text(child(node, "Interval")?))?;
    let for_minutes = minutes_of(&text(child(node, "Duration")?))?;
    (every_minutes > 0).then_some(Repeat {
        every_minutes,
        for_minutes,
    })
}

/// A whole number of minutes in an ISO 8601 duration (`PT15M`, `PT8H`, `P1D`, `PT1H30M`); `None` for
/// a duration with seconds, or one this reading does not know.
fn minutes_of(duration: &str) -> Option<u32> {
    let rest = duration.strip_prefix('P')?;
    let (days, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total = 0u32;
    for (part, in_time) in [(days, false), (time, true)] {
        let mut digits = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() {
                digits.push(c);
                continue;
            }
            let n: u32 = digits.parse().ok()?;
            digits.clear();
            let per = match (c, in_time) {
                ('D', false) => 1440,
                ('H', true) => 60,
                ('M', true) => 1,
                ('S', true) if n == 0 => 0,
                _ => return None,
            };
            total = total.checked_add(n.checked_mul(per)?)?;
        }
        if !digits.is_empty() {
            return None;
        }
    }
    Some(total)
}

fn schedule_of(node: Node<'_, '_>, at: (u8, u8), repeat: Option<Repeat>) -> Option<Trigger> {
    match node.tag_name().name() {
        "ScheduleByDay" => {
            only(node, &["DaysInterval"])?;
            let every_days = child(node, "DaysInterval").map_or(Some(1), |days| text(days).parse().ok())?;
            Some(Trigger::Daily { every_days, at, repeat })
        }
        "ScheduleByWeek" => {
            only(node, &["WeeksInterval", "DaysOfWeek"])?;
            if child(node, "WeeksInterval").is_some_and(|weeks| text(weeks) != "1") {
                return None;
            }
            let weekdays = named(child(node, "DaysOfWeek")?, &WEEKDAYS, 0)?;
            Some(Trigger::Weekly { weekdays, at, repeat })
        }
        "ScheduleByMonth" => {
            only(node, &["DaysOfMonth", "Months"])?;
            let days = elements(child(node, "DaysOfMonth")?)
                .map(|day| match (day.tag_name().name(), text(day).parse::<u8>()) {
                    ("Day", Ok(day @ 1..=31)) => Some(day),
                    _ => None,
                })
                .collect::<Option<BTreeSet<u8>>>()?;
            let months = named(child(node, "Months")?, &MONTHS, 1)?;
            (!days.is_empty()).then_some(Trigger::MonthlyDays {
                days,
                months,
                at,
                repeat,
            })
        }
        "ScheduleByMonthDayOfWeek" => {
            only(node, &["Weeks", "DaysOfWeek", "Months"])?;
            // Every occurrence: weeks 1 to 4 and the last, which is how the 5th is reached.
            let weeks: BTreeSet<String> = elements(child(node, "Weeks")?).map(text).collect();
            let every = ["1", "2", "3", "4", "Last"].map(str::to_owned);
            (weeks == BTreeSet::from(every)).then_some(())?;
            Some(Trigger::MonthlyWeekdays {
                weekdays: named(child(node, "DaysOfWeek")?, &WEEKDAYS, 0)?,
                months: named(child(node, "Months")?, &MONTHS, 1)?,
                at,
                repeat,
            })
        }
        _ => None,
    }
}

/// The numbers, counted from `first`, of the elements of `node`, each named in `table`: a month
/// set or a weekday set. `None` for an unknown name or an empty set.
fn named(node: Node<'_, '_>, table: &[&str], first: u8) -> Option<BTreeSet<u8>> {
    let set = elements(node)
        .map(|element| {
            let at = table.iter().position(|name| *name == element.tag_name().name())?;
            u8::try_from(at).ok().map(|at| at + first)
        })
        .collect::<Option<BTreeSet<u8>>>()?;
    (!set.is_empty()).then_some(set)
}
