//! What the demonstration tests of the three platforms share: a `goetia.yaml` written for a task, the
//! `goetia` binary run against it, and the task's entry in the `--json` document read back.
//!
//! The demonstration is the owner's end-to-end check: a manifest with `type: task` and a cron `schedule`
//! is installed and enabled with the real binary, fires by itself, shows its output through `run`, reports
//! its last result and next run through `status`, and is removed by `uninstall`. A platform's test owns
//! the order of those steps and how `next-run` is judged there; this module owns writing and reading.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

use serde::Serialize;
use serde_json::Value;

use super::cmd::{Run, try_run_command};
use super::list_json;

/// A POSIX `TZ` that `jiff` reads on every platform: five and a half hours ahead of UTC, in no zone database.
pub const ZONE: &str = "ABC-5:30";
/// The offset [`ZONE`] prints.
pub const ZONE_OFFSET: &str = "+05:30";

// The manifest ========================================================================================================

/// The `goetia.yaml` of one task, in a temporary directory that is removed when this is dropped.
///
/// Written through `yaml_serde`, so the YAML quoting of `* * * * *` is the library's. Every field that is
/// interpolated has `$` doubled to `$$`: every `$` that is not `${...}` or `$$` is an error (the README's
/// "Interpolation"), and the scripts the tests run use `$(...)`, `$((...))` and `$n`. `id` is not
/// interpolated, so it is written as it is. The library knows nothing of interpolation, and interpolation
/// nothing of YAML.
pub struct Manifest {
    pub id: String,
    pub command: Vec<String>,
    pub schedule: String,
    pub logs: Option<PathBuf>,
    dir: tempfile::TempDir,
}

#[derive(Serialize)]
struct Document {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    command: Vec<String>,
    schedule: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    logs: Option<String>,
}

/// `text` as a manifest string: a literal `$` is written `$$`.
fn literal(text: &str) -> String {
    text.replace('$', "$$")
}

impl Manifest {
    pub fn new(id: &str, command: Vec<String>, schedule: &str) -> Self {
        Manifest {
            id: id.to_string(),
            command,
            schedule: schedule.to_string(),
            logs: None,
            dir: tempfile::tempdir().expect("a manifest directory"),
        }
    }

    /// The directory `install -f` is given.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// `goetia.yaml`, as the fields stand now. Call again after changing a field: the file is replaced.
    pub fn write(&self) -> &Path {
        let document = Document {
            id: self.id.clone(),
            kind: "task",
            command: self.command.iter().map(|arg| literal(arg)).collect(),
            schedule: literal(&self.schedule),
            logs: self.logs.as_ref().map(|path| literal(&path.display().to_string())),
        };
        let yaml = yaml_serde::to_string(&document).expect("a manifest serializes");
        std::fs::write(self.dir.path().join("goetia.yaml"), yaml).expect("write goetia.yaml");
        self.dir.path()
    }
}

// The binary ==========================================================================================================

/// The `goetia` binary of this build, run as a child. `TZ` of the child is the one thing a test sets.
pub struct Cli {
    zone: Option<&'static str>,
}

impl Cli {
    pub fn new() -> Self {
        Cli { zone: None }
    }

    /// A `goetia` whose `TZ` is `zone`, for the reads that print times.
    pub fn in_zone(zone: &'static str) -> Self {
        Cli { zone: Some(zone) }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_goetia"));
        command.args(args);
        if let Some(zone) = self.zone {
            command.env("TZ", zone);
        }
        command
    }

    /// The child's whole output.
    pub fn output(&self, args: &[&str]) -> Output {
        self.command(args)
            .output()
            .unwrap_or_else(|e| panic!("spawn goetia {args:?}: {e}"))
    }

    pub fn run(&self, args: &[&str]) -> Run {
        try_run_command(&mut self.command(args)).unwrap_or_else(|e| panic!("{e}"))
    }

    /// `goetia daemon <args>`, which must exit 0.
    #[track_caller]
    pub fn daemon(&self, args: &[&str]) -> Run {
        let mut full = vec!["daemon"];
        full.extend_from_slice(args);
        self.run(&full).expect_ok()
    }

    /// `daemon install -f <dir>` of `manifest`, written now.
    #[track_caller]
    pub fn install(&self, manifest: &Manifest) -> Run {
        let dir = manifest.write();
        self.daemon(&["install", "-f", &dir.display().to_string()])
    }

    /// The one entry of `daemon status <id> --json`, which must exit 0 with nothing in `errors`.
    #[track_caller]
    pub fn status_json(&self, id: &str) -> Value {
        let run = self.daemon(&["status", id, "--json"]);
        let doc: Value = serde_json::from_str(&run.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {run}"));
        assert_eq!(doc["errors"], serde_json::json!([]), "{run}");
        let [entry] = doc["daemons"].as_array().map(Vec::as_slice).unwrap_or_default() else {
            panic!("`status {id}` has one entry: {run}");
        };
        entry.clone()
    }

    /// The line of `daemon status <id>`, which must exit 0.
    #[track_caller]
    pub fn status_text(&self, id: &str) -> String {
        let run = self.daemon(&["status", id]);
        let mut lines = run.stdout.lines();
        let (Some(line), None) = (lines.next(), lines.next()) else {
            panic!("`status {id}` is one line: {run}");
        };
        line.to_string()
    }

    /// `id` is not installed: `daemon status <id> --json` exits 1 with a `not-installed` error.
    #[track_caller]
    pub fn assert_not_installed(&self, id: &str) {
        let gone = self.run(&["daemon", "status", id, "--json"]);
        assert_eq!(gone.code, Some(1), "{gone}");
        let doc: Value = serde_json::from_str(&gone.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {gone}"));
        assert_eq!(doc["errors"][0]["kind"], "not-installed", "{gone}");
    }

    /// `id`'s entry in `daemon list --json`.
    #[track_caller]
    pub fn listed(&self, id: &str) -> Value {
        let listed = list_json::list_json(|args| self.output(args));
        listed.assert_own(&[id]);
        listed.doc["daemons"]
            .as_array()
            .and_then(|entries| entries.iter().find(|entry| entry["id"] == id))
            .unwrap_or_else(|| panic!("`{id}` is listed: {}", listed.context))
            .clone()
    }
}

// The document ========================================================================================================

/// Whether a field of a task's entry holds a value. `Any` is for a field the step cannot know: it is not judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Time {
    Present,
    Null,
    Any,
}

/// What `last-result` holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LastResult {
    Null,
    Code(i64),
}

/// What must hold of a task's entry at one step. The times that are `Present` are judged by the platform's
/// test, against its own oracle ([`instant`] reads them).
#[derive(Clone, Copy, Debug)]
pub struct Expect {
    pub state: &'static str,
    pub enabled: bool,
    pub pid: Time,
    pub last_result: LastResult,
    pub last_run: Time,
    pub next_run: Time,
}

/// A task that is installed and disarmed and has never run: idle, no pid, no result, no time.
pub const DISARMED: Expect = Expect {
    state: "idle",
    enabled: false,
    pid: Time::Null,
    last_result: LastResult::Null,
    last_run: Time::Null,
    next_run: Time::Null,
};

/// `entry` of the task `id` is as `expect` says. A time that is present is RFC 3339 here; its value is not judged.
#[track_caller]
pub fn assert_task_fields(entry: &Value, id: &str, expect: Expect) {
    assert_eq!(entry["id"], id, "{entry}");
    assert_eq!(entry["type"], "task", "{entry}");
    assert_eq!(entry["state"], expect.state, "{entry}");
    assert_eq!(entry["enabled"], expect.enabled, "{entry}");
    let presence = |key: &str| match entry.get(key) {
        Some(Value::Null) => Time::Null,
        Some(_) => Time::Present,
        None => panic!("`{key}` is never absent: {entry}"),
    };
    for (key, want) in [
        ("pid", expect.pid),
        ("last-run", expect.last_run),
        ("next-run", expect.next_run),
    ] {
        let got = presence(key);
        assert!(
            want == Time::Any || got == want,
            "{key} is {got:?}, not {want:?}: {entry}"
        );
    }
    match expect.last_result {
        LastResult::Null => assert_eq!(entry["last-result"], Value::Null, "{entry}"),
        LastResult::Code(code) => assert_eq!(entry["last-result"], serde_json::json!({ "code": code }), "{entry}"),
    }
    for key in ["last-run", "next-run"] {
        if let Some(text) = entry[key].as_str() {
            instant(text);
        }
    }
}

/// The instant an RFC 3339 `text` names. Panics on text that is not RFC 3339 with an offset.
#[track_caller]
pub fn instant(text: &str) -> SystemTime {
    text.parse::<jiff::Timestamp>()
        .unwrap_or_else(|e| panic!("`{text}` is not RFC 3339: {e}"))
        .into()
}

// The zone ============================================================================================================

/// A daily schedule whose time is about 12 hours from now, in the machine's local zone: the zone the
/// manager reads a schedule in (the test's process sets no `TZ`). The armed task has a next fire that far
/// away and nothing fires while it is read. A clock step of 12 hours is outside what the lanes do.
pub fn daily_in_half_a_day() -> String {
    assert!(
        std::env::var_os("TZ").is_none(),
        "the test process has a `TZ`, and the manager reads a schedule in /etc/localtime, not in `TZ`"
    );
    let at = jiff::Zoned::now()
        .checked_add(jiff::Span::new().hours(12))
        .expect("12 hours from now");
    format!("{} {} * * *", at.minute(), at.hour())
}

/// What one form of the task's status reported, read with the child's `TZ` set to [`ZONE`]. A time is
/// `None` where the form shows none (the text form writes `-`). `window` is the clock before and after
/// this form's read: the reads are separate, and a minute boundary may fall between any two.
#[derive(Debug)]
pub struct Read {
    pub form: &'static str,
    pub state: String,
    pub last_run: Option<String>,
    pub next_run: Option<String>,
    pub window: (SystemTime, SystemTime),
}

impl Read {
    /// Every time shown is at the offset of `TZ`. CI hosts run in UTC, so this is where a caller of the
    /// system zone is seen not to pass UTC.
    fn at_the_zone(self) -> Read {
        for time in [&self.last_run, &self.next_run].into_iter().flatten() {
            assert!(
                time.ends_with(ZONE_OFFSET),
                "`{time}` is not at {ZONE_OFFSET}: {self:?}"
            );
        }
        self
    }

    fn of_entry(form: &'static str, entry: &Value, window: (SystemTime, SystemTime)) -> Read {
        let field = |key: &str| entry[key].as_str().map(str::to_string);
        Read {
            form,
            state: entry["state"].as_str().expect("a state").to_string(),
            last_run: field("last-run"),
            next_run: field("next-run"),
            window,
        }
        .at_the_zone()
    }

    fn of_text(line: &str, window: (SystemTime, SystemTime)) -> Read {
        // The text form writes `-` for a time it has none of.
        let word = |key: &str| Some(text_field(line, key).to_string()).filter(|word| word != "-");
        Read {
            form: "status",
            state: text_state(line).to_string(),
            last_run: word("last-run"),
            next_run: word("next-run"),
            window,
        }
        .at_the_zone()
    }
}

/// `read`, and the clock before and after it.
fn windowed<T>(read: impl FnOnce() -> T) -> (T, (SystemTime, SystemTime)) {
    let before = SystemTime::now();
    let value = read();
    (value, (before, SystemTime::now()))
}

/// The three forms that print a task's times, each read once, at the offset of the child's `TZ`.
#[derive(Debug)]
pub struct Zoned {
    /// The `status --json` entry, read by `json_read`.
    pub json: Value,
    pub json_read: Read,
    pub text_read: Read,
    pub list_read: Read,
}

/// `id` read once in each form through `in_zone`, a [`Cli::in_zone`] of [`ZONE`].
pub fn read_in_the_zone(in_zone: &Cli, id: &str) -> Zoned {
    let (json, json_window) = windowed(|| in_zone.status_json(id));
    let (text, text_window) = windowed(|| in_zone.status_text(id));
    let (listed, list_window) = windowed(|| in_zone.listed(id));
    Zoned {
        json_read: Read::of_entry("status --json", &json, json_window),
        text_read: Read::of_text(&text, text_window),
        list_read: Read::of_entry("list --json", &listed, list_window),
        json,
    }
}

impl Zoned {
    pub fn reads(&self) -> [&Read; 3] {
        [&self.json_read, &self.text_read, &self.list_read]
    }

    /// Every form is in `state` with no `next-run`.
    #[track_caller]
    pub fn assert_state(&self, state: &str) {
        for read in self.reads() {
            assert_eq!(read.state, state, "{}: {self:?}", read.form);
            assert_eq!(read.next_run.as_deref().map(instant), None, "{}: {self:?}", read.form);
        }
    }

    /// Every form is in `state`, and its `next-run` is one of `candidates(before, after)`, the clock
    /// before and after that form's own read.
    #[track_caller]
    pub fn assert_next_run(&self, state: &str, candidates: impl Fn(SystemTime, SystemTime) -> Vec<SystemTime>) {
        for read in self.reads() {
            assert_eq!(read.state, state, "{}: {self:?}", read.form);
            let next = read.next_run.as_deref().map(instant);
            let (before, after) = read.window;
            let expected = candidates(before, after);
            assert!(
                next.is_some_and(|next| expected.contains(&next)),
                "{}: next-run {next:?} is none of {expected:?}, read in [{before:?}, {after:?}]: {self:?}",
                read.form
            );
        }
    }

    /// No form shows a `last-run`.
    #[track_caller]
    pub fn assert_no_last_run(&self) {
        for read in self.reads() {
            assert!(read.last_run.is_none(), "{}: a last-run: {self:?}", read.form);
        }
    }
}

/// The state of a status text line (`id: state (task, ...)`).
#[track_caller]
pub fn text_state(line: &str) -> &str {
    line.split_once(": ")
        .and_then(|(_, rest)| rest.split_once(" ("))
        .map(|(state, _)| state)
        .unwrap_or_else(|| panic!("`{line}` is not a status line"))
}

/// The value of the key `key` of a status text line (`id: state (task, enabled=true, ..., key=value, ...)`),
/// `-` where the line has none.
#[track_caller]
pub fn text_field<'a>(line: &'a str, key: &str) -> &'a str {
    let start = line
        .find(&format!("{key}="))
        .unwrap_or_else(|| panic!("`{line}` has no `{key}`"))
        + key.len()
        + 1;
    let rest = &line[start..];
    let end = rest.find([',', ')']).unwrap_or(rest.len());
    &rest[..end]
}
