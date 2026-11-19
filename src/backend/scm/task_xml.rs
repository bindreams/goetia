//! The Task Scheduler definition of a task: the XML goetia registers, and the definition read back
//! from what Task Scheduler stores. Pure, so it builds and is tested on every host.
//!
//! A task is one `Exec` action, the shim host in task mode, and the blob in a top-level `<Data>`
//! (`Data` inside `RegistrationInfo` is refused; Task Scheduler drops every CR and the outer
//! whitespace of it, so the blob is LF-only text with none). What Task Scheduler stores is not what
//! was written: it reorders elements, drops values equal to its defaults, adds `URI` and
//! `UseUnifiedSchedulingEngine`, and rewrites the principal's account to a SID. So a readback is a
//! [`TaskXml`] built from the elements goetia owns, with a dropped one read as its default, and the
//! account is the blob's, never the principal's. [`Display`](fmt::Display) is the definition's
//! canonical text: the same for what is generated and what is read back, with no registration
//! [`Token`] ([`TaskXml::token_of`]).
//!
//! **A task is armed when it has triggers that are switched on** ([`TaskXml::armed`]; its own
//! `Settings/Enabled` stays true, since Task Scheduler refuses a `Run` of a task whose own flag is false).
//! A disarmed task has none: disabling a trigger would leave a repetition in progress firing, and
//! removing the triggers stops it. So [`TaskXml::to_xml`] writes the triggers only for an armed task, and
//! what is read back of a task with none is the definition it would arm: the triggers its blob's
//! schedule compiles to. The edits of the stored XML:
//! - `disable`: [`TaskXml::without_triggers`];
//! - `enable`, no trigger on: [`TaskXml::with_triggers`];
//! - `enable`, a trigger on: [`TaskXml::with_own_flag`].

use std::collections::BTreeSet;
use std::fmt;

use cosca::quote::windows::join_wide;

use crate::backend::scm::instance::{TaskAccount, bytes_of_hex, hex_of};
use crate::backend::scm::triggers::{Repeat, Trigger, compile};
use crate::backend::xml_escape;
use crate::blob::{self, Blob};
use crate::error::Error;
use crate::shim::HostCommand;
use crate::spec::{AccountId, Builtin, DaemonSpec, Id, User, invalid, windows_builtin};

mod read;

/// The argument that makes the shim run as a task: the action's arguments are the host's own, then
/// this, the task's id and the registration's [`Token`].
pub(crate) const TASK_MODE_ARG: &str = "--goetia-task";

/// What tells one registration of a task from another: the last argument of its action. A create makes a
/// fresh one and an update keeps it, so a shim started under a registration that has since been
/// replaced finds another in the task it reads, and starts no program. It is no part of a
/// [`TaskXml`]: what an install compares is the definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Token([u8; 16]);

impl Token {
    /// A token no earlier one equals: 128 random bits.
    pub(crate) fn fresh() -> Token {
        Token(rand::random::<u128>().to_be_bytes())
    }

    /// `text` as a token: exactly 32 hex digits.
    pub(crate) fn parse(text: &str) -> Option<Token> {
        bytes_of_hex(text).map(Token)
    }

    /// The 32 lower-case hex digits the action's arguments end with.
    pub(crate) fn hex(&self) -> String {
        hex_of(&self.0)
    }
}

/// What the stored XML of a task says of its registration ([`TaskXml::stored`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Stored {
    /// The account its principal names ([`TaskXml::principal`]).
    pub principal: Option<TaskAccount>,
    /// Whether it is armed ([`TaskXml::armed`]).
    pub armed: bool,
    /// The token its arguments end with, `None` when they end with no goetia ending for the task
    /// ([`TaskXml::token_of`]).
    pub token: Option<Token>,
}

/// Where a schedule that never fires is anchored: a one-time trigger at a fixed past instant fires
/// nothing at registration or when it is enabled, and reads no next run.
pub(crate) const NEVER_AT: &str = "2000-01-01T00:00:00";

/// Every calendar trigger starts at its time of day on this date: a past instant, so that nothing
/// fires at registration, and one whose weekday and day of month the triggers never depend on.
const START_DATE: &str = "2000-01-01";

const SYSTEM: &str = "S-1-5-18";
const LOCAL_SERVICE: &str = "S-1-5-19";
const NETWORK_SERVICE: &str = "S-1-5-20";

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

/// Sunday first, as cron numbers the days.
const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

// Settings ============================================================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// A child of `Principals/Principal`.
    Principal,
    /// A child of `Settings`.
    Settings,
    /// A child of `Settings/IdleSettings`.
    Idle,
}

/// One value goetia sets, and the one Task Scheduler assumes where it leaves the element out of a
/// readback.
struct Setting {
    scope: Scope,
    name: &'static str,
    value: &'static str,
    absent: &'static str,
}

const fn setting(scope: Scope, name: &'static str, value: &'static str, absent: &'static str) -> Setting {
    Setting {
        scope,
        name,
        value,
        absent,
    }
}

/// The settings of a task: no run is bounded in time, a run that is alive makes a new start a no-op,
/// a start missed while the machine was off runs when it is back (one that came due while the task was
/// disarmed is not made up), and the battery, idle, network and wake flags are
/// off. `Priority` 5 gives the normal class a service runs at; the default 7 is below normal.
const SETTINGS: [Setting; 16] = [
    setting(Scope::Principal, "RunLevel", "HighestAvailable", "LeastPrivilege"),
    setting(Scope::Settings, "MultipleInstancesPolicy", "IgnoreNew", "IgnoreNew"),
    setting(Scope::Settings, "DisallowStartIfOnBatteries", "false", "true"),
    setting(Scope::Settings, "StopIfGoingOnBatteries", "false", "true"),
    setting(Scope::Settings, "AllowHardTerminate", "true", "true"),
    setting(Scope::Settings, "StartWhenAvailable", "true", "false"),
    setting(Scope::Settings, "RunOnlyIfNetworkAvailable", "false", "false"),
    setting(Scope::Idle, "StopOnIdleEnd", "false", "true"),
    setting(Scope::Idle, "RestartOnIdle", "false", "false"),
    setting(Scope::Settings, "AllowStartOnDemand", "true", "true"),
    setting(Scope::Settings, "Enabled", "true", "true"),
    setting(Scope::Settings, "Hidden", "false", "false"),
    setting(Scope::Settings, "RunOnlyIfIdle", "false", "false"),
    setting(Scope::Settings, "WakeToRun", "false", "false"),
    setting(Scope::Settings, "ExecutionTimeLimit", "PT0S", "PT72H"),
    setting(Scope::Settings, "Priority", "5", "7"),
];

// TaskXml =============================================================================================================

/// What goetia registers for a task, or reads back of one: the action, the account, the triggers,
/// the settings and the blob. Whether the task is armed is no part of it: see [`TaskXml::to_xml`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskXml {
    id: Id,
    /// The spec's `name`, written as the task's description.
    name: String,
    /// The action without the task-mode arguments.
    host: HostCommand,
    account: TaskAccount,
    triggers: Vec<Trigger>,
    /// The blob's text, [`blob::metadata_text`].
    data: String,
    /// The value of each of [`SETTINGS`], in its order.
    settings: Vec<String>,
    /// What a readback holds in its triggers or its action that goetia does not write, in one line
    /// each: a hand edit, which [`Display`](fmt::Display) shows.
    unrecognised: Vec<String>,
}

impl TaskXml {
    /// The definition of the task `spec`, run through `host`.
    ///
    /// Refused with [`Error::Invalid`]: a virtual service account (Task Scheduler runs a task
    /// registered with one as LOCAL SERVICE), a `%` in the host's program or arguments (Task
    /// Scheduler expands `%VAR%` in a task's arguments), and what [`compile`] refuses.
    ///
    /// The account is read from the spec alone, so that a readback can say the same.
    pub(crate) fn new(spec: &DaemonSpec, host: HostCommand) -> Result<TaskXml, Error> {
        let common = spec.common();
        let account = account_of(&common.id, &common.user)?;
        refuse_percent(&common.id, &host)?;
        let triggers = triggers_of(spec)?;
        Ok(TaskXml {
            id: common.id.clone(),
            name: common.name.clone(),
            host,
            account,
            triggers,
            data: blob::metadata_text(spec),
            settings: SETTINGS.iter().map(|setting| setting.value.to_owned()).collect(),
            unrecognised: Vec::new(),
        })
    }

    /// The action's program and arguments without the task-mode arguments: the host.
    pub(crate) fn host(&self) -> HostCommand {
        self.host.clone()
    }

    /// The account, which says which logon type the registration needs.
    pub(crate) fn account(&self) -> TaskAccount {
        self.account.clone()
    }

    /// The XML to register. The triggers are written only when `armed`, which is the installation's
    /// state and no part of the definition: an install over a disarmed task keeps it without any.
    /// `token` is the registration's, the action's last argument.
    pub(crate) fn to_xml(&self, armed: bool, token: &Token) -> String {
        let mut out = String::new();
        out.push_str("<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n");
        out.push_str("<Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">");
        out.push_str(&format!(
            "<RegistrationInfo><Description>{}</Description></RegistrationInfo>",
            xml_escape(&self.name)
        ));
        let (user, logon) = match &self.account {
            TaskAccount::Service(name) => (name, ""),
            TaskAccount::User(name) => (name, "<LogonType>Password</LogonType>"),
        };
        out.push_str(&format!(
            "<Principals><Principal id=\"Author\"><UserId>{}</UserId>{logon}<RunLevel>{}</RunLevel></Principal></Principals>",
            xml_escape(user),
            self.value("RunLevel")
        ));
        out.push_str(&self.settings_xml());
        out.push_str(&format!("<Data>{}</Data>", xml_escape(&self.data)));
        if armed {
            out.push_str(&triggers_xml(&self.triggers));
        }
        out.push_str(&format!(
            "<Actions Context=\"Author\"><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions>",
            xml_escape(&self.host.exe.to_string_lossy()),
            xml_escape(&self.arguments(token))
        ));
        out.push_str("</Task>");
        out
    }

    /// The value of the setting `name`.
    fn value(&self, name: &str) -> &str {
        let at = SETTINGS
            .iter()
            .position(|setting| setting.name == name)
            .expect("a setting named in the code is in the table");
        &self.settings[at]
    }

    fn settings_xml(&self) -> String {
        let mut out = String::from("<Settings>");
        let mut idle = false;
        for (setting, value) in SETTINGS.iter().zip(&self.settings) {
            let element = format!("<{0}>{1}</{0}>", setting.name, xml_escape(value));
            match setting.scope {
                Scope::Principal => {}
                Scope::Idle => {
                    if !idle {
                        out.push_str("<IdleSettings>");
                        idle = true;
                    }
                    out.push_str(&element);
                }
                Scope::Settings => {
                    if idle {
                        out.push_str("</IdleSettings>");
                        idle = false;
                    }
                    out.push_str(&element);
                }
            }
        }
        if idle {
            out.push_str("</IdleSettings>");
        }
        out.push_str("</Settings>");
        out
    }

    /// The action's `Arguments`: the host's, the task-mode argument, the id and the token, quoted as
    /// `CommandLineToArgvW` reads them back (they arrive verbatim after the quoted command).
    fn arguments(&self, token: &Token) -> String {
        let wide: Vec<Vec<u16>> = self
            .host
            .args
            .iter()
            .map(String::as_str)
            .chain([TASK_MODE_ARG, self.id.as_str(), token.hex().as_str()])
            .map(|arg| arg.encode_utf16().collect())
            .collect();
        let borrowed: Vec<&[u16]> = wide.iter().map(Vec::as_slice).collect();
        String::from_utf16(&join_wide(&borrowed)).expect("UTF-16 made from `str`s is valid")
    }
}

// The account ---------------------------------------------------------------------------------------------------------

/// The SID text of a well-known account, as Task Scheduler reads it back.
pub(in crate::backend::scm) fn builtin_sid(builtin: Builtin) -> &'static str {
    match builtin {
        Builtin::LocalSystem => SYSTEM,
        Builtin::LocalService => LOCAL_SERVICE,
        Builtin::NetworkService => NETWORK_SERVICE,
    }
}

/// The account a task runs as, from the spec's user. Well-known accounts are named by SID, as Task
/// Scheduler reads them back.
fn account_of(id: &Id, user: &User) -> Result<TaskAccount, Error> {
    let name = match user {
        User::Root => return Ok(TaskAccount::Service(SYSTEM.to_owned())),
        User::Name(name) | User::Id(AccountId::Sid(name)) => name,
        User::Id(AccountId::Uid(uid)) => {
            return Err(invalid(
                id,
                &format!("user.id `{uid}` is a numeric UID, which Windows has no account for"),
            ));
        }
    };
    let sid = match windows_builtin(name) {
        Some(builtin) => Some(builtin_sid(builtin)),
        None => [SYSTEM, LOCAL_SERVICE, NETWORK_SERVICE]
            .into_iter()
            .find(|sid| sid.eq_ignore_ascii_case(name)),
    };
    if let Some(sid) = sid {
        return Ok(TaskAccount::Service(sid.to_owned()));
    }
    let lower = name.to_ascii_lowercase();
    if lower.starts_with(r"nt service\") || lower.starts_with("s-1-5-80-") {
        return Err(invalid(
            id,
            &format!(
                "`{name}` is a virtual service account, and Task Scheduler runs a task registered with one as LOCAL \
                 SERVICE, so a task cannot run as it; use `LocalService`, `NetworkService`, `root` or a user account"
            ),
        ));
    }
    Ok(TaskAccount::User(name.clone()))
}

/// The name of a well-known account for the definition's text.
fn service_label(sid: &str) -> &str {
    match sid {
        SYSTEM => "SYSTEM",
        LOCAL_SERVICE => "LocalService",
        NETWORK_SERVICE => "NetworkService",
        other => other,
    }
}

/// Task Scheduler expands `%VAR%` in a task's arguments, so no escaping makes a `%` arrive as written.
fn refuse_percent(id: &Id, host: &HostCommand) -> Result<(), Error> {
    let refuse = |what: &str| {
        Err(invalid(
            id,
            &format!(
                "`{what}` contains `%`, which Task Scheduler expands in a task's arguments; a task's program and \
                 arguments cannot contain `%`"
            ),
        ))
    };
    if host.exe.to_string_lossy().contains('%') {
        return refuse("the shim host's path");
    }
    if host.args.iter().any(|arg| arg.contains('%')) {
        return refuse("the shim host's arguments");
    }
    Ok(())
}

// The triggers --------------------------------------------------------------------------------------------------------

/// The triggers `spec`'s schedule compiles to: none for a task with no schedule.
pub(crate) fn triggers_of(spec: &DaemonSpec) -> Result<Vec<Trigger>, Error> {
    let DaemonSpec::Task(task) = spec else {
        return Err(Error::blob(format!(
            "`{}` is a service, not a task, so it has no triggers",
            spec.common().id
        )));
    };
    match &task.schedule {
        Some(schedule) => compile(schedule, &spec.common().id),
        None => Ok(Vec::new()),
    }
}

/// The `Triggers` element of `triggers`, empty text for none.
fn triggers_xml(triggers: &[Trigger]) -> String {
    if triggers.is_empty() {
        return String::new();
    }
    let inner: String = triggers.iter().map(trigger_xml).collect();
    format!("<Triggers>{inner}</Triggers>")
}

fn trigger_xml(trigger: &Trigger) -> String {
    let (at, repeat, schedule) = match trigger {
        Trigger::Never => {
            return format!("<TimeTrigger><StartBoundary>{NEVER_AT}</StartBoundary></TimeTrigger>");
        }
        Trigger::Daily { every_days, at, repeat } => (
            at,
            repeat,
            format!("<ScheduleByDay><DaysInterval>{every_days}</DaysInterval></ScheduleByDay>"),
        ),
        Trigger::Weekly { weekdays, at, repeat } => (
            at,
            repeat,
            format!(
                "<ScheduleByWeek><WeeksInterval>1</WeeksInterval><DaysOfWeek>{}</DaysOfWeek></ScheduleByWeek>",
                names(&WEEKDAYS, weekdays, 0)
            ),
        ),
        Trigger::MonthlyDays {
            days,
            months,
            at,
            repeat,
        } => (
            at,
            repeat,
            format!(
                "<ScheduleByMonth><DaysOfMonth>{}</DaysOfMonth><Months>{}</Months></ScheduleByMonth>",
                days.iter().map(|day| format!("<Day>{day}</Day>")).collect::<String>(),
                names(&MONTHS, months, 1)
            ),
        ),
        Trigger::MonthlyWeekdays {
            weekdays,
            months,
            at,
            repeat,
        } => (
            at,
            repeat,
            format!(
                "<ScheduleByMonthDayOfWeek><Weeks><Week>1</Week><Week>2</Week><Week>3</Week><Week>4</Week>\
                 <Week>Last</Week></Weeks><DaysOfWeek>{}</DaysOfWeek><Months>{}</Months></ScheduleByMonthDayOfWeek>",
                names(&WEEKDAYS, weekdays, 0),
                names(&MONTHS, months, 1)
            ),
        ),
    };
    let repetition = match repeat {
        Some(Repeat {
            every_minutes,
            for_minutes,
        }) => format!(
            "<Repetition><Interval>PT{every_minutes}M</Interval><Duration>PT{for_minutes}M</Duration></Repetition>"
        ),
        None => String::new(),
    };
    format!(
        "<CalendarTrigger><StartBoundary>{START_DATE}T{:02}:{:02}:00</StartBoundary>{repetition}{schedule}\
         </CalendarTrigger>",
        at.0, at.1
    )
}

/// The elements named by `values`, each a number counted from `first`, in `table`.
fn names(table: &[&str], values: &BTreeSet<u8>, first: u8) -> String {
    values
        .iter()
        .map(|&value| format!("<{}/>", table[usize::from(value - first)]))
        .collect()
}

// The blob ------------------------------------------------------------------------------------------------------------

/// The blob a task's `Data` carries. `Ok(None)` for a task that is not goetia's: no `Data`, or text
/// that does not start with the marker line. A marker whose blob does not decode is an error, never
/// absent. `data` has LF or CRLF line ends: Task Scheduler's `Xml` returns the CRLF form.
pub(crate) fn extract_from_data(data: &str) -> Result<Option<Blob>, Error> {
    let data = data.trim();
    if !data.starts_with("Marker:") {
        return Ok(None);
    }
    blob::parse_metadata_text(data).map(Some)
}

// Rendering ===========================================================================================================

/// The definition's canonical text, for diffing: the same for a generated definition and a readback
/// of it, with no armed state and no token. Every value goetia owns is shown, a default Task Scheduler dropped
/// included; the account is a well-known account's name or the user's.
impl fmt::Display for TaskXml {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(out, "Name: {}", self.id)?;
        match &self.account {
            TaskAccount::Service(sid) => writeln!(out, "Account: service {}", service_label(sid))?,
            TaskAccount::User(name) => writeln!(out, "Account: user {name:?}")?,
        }
        writeln!(out, "Host: {:?}", self.host.exe)?;
        writeln!(out, "Arguments:")?;
        for arg in self
            .host
            .args
            .iter()
            .map(String::as_str)
            .chain([TASK_MODE_ARG, self.id.as_str()])
        {
            writeln!(out, "  - {arg:?}")?;
        }
        // Task Scheduler does not promise the order it stores the triggers in.
        let mut triggers: Vec<String> = self.triggers.iter().map(Trigger::to_string).collect();
        triggers.sort();
        writeln!(out, "Triggers:")?;
        for trigger in &triggers {
            writeln!(out, "  - {trigger}")?;
        }
        writeln!(out, "Settings:")?;
        for (setting, value) in SETTINGS.iter().zip(&self.settings) {
            writeln!(out, "  {}: {value}", setting.name)?;
        }
        writeln!(out, "Data:")?;
        for line in self.data.lines() {
            writeln!(out, "  {line:?}")?;
        }
        if !self.unrecognised.is_empty() {
            let mut unrecognised = self.unrecognised.clone();
            unrecognised.sort();
            writeln!(out, "Unrecognised:")?;
            for text in &unrecognised {
                writeln!(out, "  {text:?}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "task_xml_tests.rs"]
mod task_xml_tests;
