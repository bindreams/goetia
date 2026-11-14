//! Pure generation of the unit file from a resolved spec, and extraction of
//! the embedded metadata blob back out of one.
//!
//! Not `#[cfg]`-gated: this compiles and its tests run on every platform.
//!
//! Escaping obligations: `%` is doubled in every emitted value, since
//! systemd treats it as a specifier prefix and a lone one makes systemd
//! refuse to load the unit outright. `ExecStart=`/`Environment=` values
//! are additionally quoted per systemd's command-line grammar when they
//! contain whitespace or a quoting-relevant character; `ExecStart=`
//! further doubles `$` in its arguments (systemd performs `$FOO`/`${FOO}` environment
//! substitution there, but not in the executable path; it does in `argv[0]`, so a path holding `$`
//! is written `@<path> <argv[0]>`), and quotes a bare `;` argument (systemd's
//! command-line separator). A task's `ExecStart=` runs its command through `sh` instead, so its path
//! is one more argument ([`render_task_exec_start`]); a control character in a word is written as an
//! escape.
//!
//! Backslashes are never refused: a quoted word doubles each, and a raw value ending in an odd
//! run of them gets a trailing space (see `raw_value`). The values no encoding can carry are
//! refused by [`units()`]: see `refuse_unrepresentable`.

use std::collections::BTreeSet;

use crate::backend::Identity;
use crate::blob::{self, Blob, MARKER};
use crate::error::Error;
use crate::spec::{
    DaemonFields, DaemonSpec, DayRule, Restart, Schedule, ServiceSpec, TaskSpec, systemd_defect,
    systemd_raw_value_defect,
};

// Generation ==========================================================================================================

/// What goetia writes for one daemon: the job's unit, and for a task with a `schedule` the timer
/// unit that starts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Units {
    /// `<id>.service`.
    pub service: String,
    /// `<id>.timer`: `None` for a service, and for a task with no `schedule`.
    pub timer: Option<String>,
}

impl std::fmt::Display for Units {
    /// The service text, then the timer text under a `# ---- timer unit ----` line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.service)?;
        if let Some(timer) = &self.timer {
            write!(f, "\n# ---- timer unit ----\n{timer}")?;
        }
        Ok(())
    }
}

/// Render `spec` as systemd unit files for the already-resolved account `id`: a service gives its
/// `.service`; a task gives its oneshot `.service` and, when it has a schedule, its `.timer`.
///
/// A service's `[Install]` section is emitted so `systemctl enable` is possible later, but
/// generation never consults enablement state: see the crate-level design notes on why
/// boot-enablement is excluded from every drift comparison.
///
/// `units(&extract(&units(spec, id).service).unwrap().unwrap().spec, id)` must be byte-identical to
/// `units(spec, id)`, and so for the timer.
///
/// Fails with [`Error::Invalid`] for the values `refuse_unrepresentable` lists.
///
/// A service emits `Type=exec` (requires systemd 240+, 2018): `systemd.service(5)` recommends it for
/// long-running services "as it ensures that process setup errors (e.g. errors such as a missing
/// service executable, or missing user) are properly tracked", which `Type=simple` does not —
/// `systemctl start` there can report success for a unit that immediately goes `failed`. The
/// accepted cost is that `exec` makes systemd block on the NSS lookup `User=` triggers before
/// reporting back; every generated unit carries `User=`, so this applies to all of them, and
/// `--timeout` bounds and reports it.
///
/// Changing what this function emits after a release ships requires bumping `version` in the same
/// commit (see `blob::encode`), or every already-installed daemon using the old bytes reports as
/// hand-edited: `decide`'s version-mismatch check (`src/decide.rs`) is what tells "stale, regenerate
/// silently" from "conflict, demand `--force`" apart, and it runs before either `desired` or
/// `regenerated` is even compared.
pub fn units(spec: &DaemonSpec, id: &Identity) -> Result<Units, Error> {
    match spec {
        DaemonSpec::Service(service) => Ok(Units {
            service: with_goetia_section(service_unit(service, id)?, spec),
            timer: None,
        }),
        DaemonSpec::Task(task) => Ok(Units {
            service: with_goetia_section(task_unit(task, id)?, spec),
            timer: timer_for(spec),
        }),
    }
}

/// The timer unit `spec` has: `None` for a service and for a task with no schedule. It needs no
/// identity, so a timer is regenerated from a spec alone.
pub fn timer_for(spec: &DaemonSpec) -> Option<String> {
    let DaemonSpec::Task(task) = spec else {
        return None;
    };
    let schedule = task.schedule.as_ref()?;
    Some(with_goetia_section(timer_unit(task, schedule), spec))
}

/// `body`, a blank line, and the `[X-Goetia]` section of `spec`.
fn with_goetia_section(mut body: String, spec: &DaemonSpec) -> String {
    body.push('\n');
    body.push_str(&goetia_section(spec));
    body
}

/// A service's unit, up to the `[X-Goetia]` section.
fn service_unit(service: &ServiceSpec, id: &Identity) -> Result<String, Error> {
    refuse_unrepresentable(&service.common, id)?;
    let mut out = String::new();

    out.push_str("[Unit]\n");
    out.push_str(&format!("Description={}\n", raw_value(&service.common.name)));
    if service.restart != Restart::Never {
        // systemd's defaults (`DefaultStartLimitIntervalSec=10s`,
        // `StartLimitBurst=5`) put a unit permanently into `failed` after
        // five restarts in ten seconds: `restart: always` would mean
        // "dies permanently after five tries" instead of restarting
        // forever, as it does on the other two platforms.
        out.push_str("StartLimitIntervalSec=0\n");
    }
    out.push('\n');

    out.push_str("[Service]\n");
    out.push_str("Type=exec\n");
    out.push_str(&exec_lines(
        &service.common,
        id,
        &render_exec_start(&service.common.command),
    ));
    out.push_str(&format!("Restart={}\n", render_restart(service.restart)));
    if let Some(delay) = service.restart_delay {
        out.push_str(&format!("RestartSec={}\n", delay.as_secs_f64()));
    }
    out.push_str(&log_lines(&service.common));
    out.push('\n');

    out.push_str("[Install]\n");
    out.push_str("WantedBy=multi-user.target\n");

    Ok(out)
}

/// A task's oneshot unit, up to the `[X-Goetia]` section. A failed run ends and waits for the next
/// one: no `Restart=`. No `RemainAfterExit=`, so a finished run is `inactive`. No `[Install]`: the
/// timer, not boot, starts it.
///
/// Its command runs through `sh`, which prints the run's marker first ([`render_task_exec_start`]).
/// No `LogRateLimit*` line: a per-unit limit does not lift journald's (measured), so it would only
/// look like a guarantee.
fn task_unit(task: &TaskSpec, id: &Identity) -> Result<String, Error> {
    refuse_unrepresentable(&task.common, id)?;
    refuse_env_sh_resets(&task.common)?;
    let mut out = String::new();

    out.push_str("[Unit]\n");
    out.push_str(&format!("Description={}\n", raw_value(&task.common.name)));
    // A burst of starts (`goetia daemon run` in a loop, a timer catching up after downtime) must
    // not make systemd refuse the next one: the default allows five in ten seconds.
    out.push_str("StartLimitIntervalSec=0\n");
    out.push('\n');

    out.push_str("[Service]\n");
    out.push_str("Type=oneshot\n");
    out.push_str(&exec_lines(
        &task.common,
        id,
        &render_task_exec_start(&task.common.command),
    ));
    out.push_str(&log_lines(&task.common));

    Ok(out)
}

/// A task's timer unit, up to the `[X-Goetia]` section. It names no `Unit=`: the default is the
/// service of the same name.
///
/// `AccuracySec=1s` makes the run start within the cron second; the default of one minute fired
/// 11 to 25 s late when measured. `Persistent=yes` runs a start the machine missed once it is up.
fn timer_unit(task: &TaskSpec, schedule: &Schedule) -> String {
    let mut out = String::new();

    out.push_str("[Unit]\n");
    out.push_str(&format!("Description={}\n", raw_value(&task.common.name)));
    out.push('\n');

    out.push_str("[Timer]\n");
    for line in on_calendar_lines(schedule) {
        out.push_str(&format!("OnCalendar={line}\n"));
    }
    out.push_str("AccuracySec=1s\n");
    out.push_str("Persistent=yes\n");
    out.push('\n');

    out.push_str("[Install]\n");
    out.push_str("WantedBy=timers.target\n");

    out
}

/// `[Service]`'s lines from `ExecStart=` through `User=`, which a service and a task share.
fn exec_lines(common: &DaemonFields, id: &Identity, exec_start: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("ExecStart={exec_start}\n"));
    if let Some(cwd) = &common.cwd {
        out.push_str(&format!("WorkingDirectory={}\n", raw_value(&cwd.display().to_string())));
    }
    for (key, value) in &common.env {
        out.push_str(&format!("Environment={}\n", render_environment(key, value)));
    }
    out.push_str(&format!("User={}\n", raw_value(&id.user)));
    out
}

/// `[Service]`'s log lines, which a service and a task share. A service puts `Restart=` between
/// [`exec_lines`] and these, so its bytes stay as they were.
fn log_lines(common: &DaemonFields) -> String {
    let mut out = String::new();
    if let Some(logs) = &common.logs {
        let target = format!("append:{}", raw_value(&logs.display().to_string()));
        out.push_str(&format!("StandardOutput={target}\n"));
        out.push_str(&format!("StandardError={target}\n"));
    }
    out
}

/// The `[X-Goetia]` section both units carry: the marker and the spec's blob.
fn goetia_section(spec: &DaemonSpec) -> String {
    let mut out = String::new();
    out.push_str("[X-Goetia]\n");
    out.push_str(&format!("Marker={MARKER}\n"));
    out.push_str(&format!("Schema={}\n", blob::SCHEMA));
    out.push_str(&format!("Version={}\n", crate::version()));
    out.push_str(&format!("Spec={}\n", blob::encode(spec)));
    out
}

// OnCalendar ==========================================================================================================

/// systemd spells day-of-week 0 to 6 `Sun` to `Sat`.
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// The `OnCalendar=` values (each without the key) that fire when `schedule` does.
///
/// Every value of every field is listed, two digits each, and a field that holds its whole range is
/// `*`. Cron's two day fields are one line when both must match (systemd's own rule for a weekday
/// and a day: AND), and two when either may (`DayRule::Or`): the days of the month on one, the
/// weekdays on the other, the time and month on both.
fn on_calendar_lines(schedule: &Schedule) -> Vec<String> {
    let [minute, hour, day, month, weekday] = schedule.fields();
    let time = format!("{}:{}:00", numbers(&hour.values, 24), numbers(&minute.values, 60));
    let month = numbers(&month.values, 12);
    let weekdays = if weekday.values.len() == WEEKDAYS.len() {
        String::new()
    } else {
        let names: Vec<&str> = weekday.values.iter().map(|&v| WEEKDAYS[usize::from(v)]).collect();
        format!("{} ", names.join(","))
    };
    let days = numbers(&day.values, 31);
    match schedule.day_rule() {
        DayRule::And => vec![format!("{weekdays}*-{month}-{days} {time}")],
        DayRule::Or => vec![
            format!("*-{month}-{days} {time}"),
            format!("{weekdays}*-{month}-* {time}"),
        ],
    }
}

/// `values` as systemd reads a calendar component: `*` when it holds all `range_len` values of its
/// field, else each in two digits, comma separated.
fn numbers(values: &BTreeSet<u8>, range_len: usize) -> String {
    if values.len() == range_len {
        return "*".to_string();
    }
    let digits: Vec<String> = values.iter().map(|v| format!("{v:02}")).collect();
    digits.join(",")
}

/// Refuse what no encoding can carry: the manifest's values ([`systemd_defect`], which lists each rule
/// with its measurement, and which `resolve` runs too, since a `DaemonSpec` can come from a blob that
/// never passed it) and the `User=` identity. The list is what is refused, not everything systemd
/// cannot carry.
fn refuse_unrepresentable(common: &DaemonFields, id: &Identity) -> Result<(), Error> {
    let defect = systemd_defect(common).or_else(|| systemd_raw_value_defect("User=", &id.user));
    match defect {
        Some(message) => Err(Error::Invalid {
            daemon: common.id.to_string(),
            message,
            source: None,
        }),
        None => Ok(()),
    }
}

/// What a task's run prints first, followed by the run's invocation id: `run` finds its run's
/// stream in the journal by the line `<RUN_MARKER> <INVOCATION_ID>`.
pub(crate) const RUN_MARKER: &str = "goetia-run";

/// The variables `/bin/sh` resets or removes on its way to the program (measured on dash and bash
/// 5.2: the KB note "A sh exec wrapper adds PWD, and under bash SHLVL=0, to the program's environment
/// and resets IFS"), so a task cannot pass them.
const RESET_BY_SH: [&str; 6] = ["IFS", "PWD", "SHLVL", "PS4", "_", "OLDPWD"];

/// Refuse an `env` of a task that sets a variable `sh` resets: the program would not see the value.
fn refuse_env_sh_resets(common: &DaemonFields) -> Result<(), Error> {
    match RESET_BY_SH.iter().find(|name| common.env.contains_key(**name)) {
        Some(name) => Err(Error::Invalid {
            daemon: common.id.to_string(),
            message: format!(
                "`env` sets `{name}`, but a task starts through /bin/sh, which resets it, so the program \
                 would not see the value"
            ),
            source: None,
        }),
        None => Ok(()),
    }
}

/// A task's `ExecStart=` value: `/bin/sh` prints [`RUN_MARKER`] and the run's `INVOCATION_ID`, then
/// execs the command. `command` follows as the script's `$0` (the program, so its `argv[0]` is the
/// path) and `$@`, each word written as an `ExecStart=` argument: through `sh` the path is an
/// ordinary word, so it gets none of the `@path argv0` form a direct `ExecStart=` needs. The script
/// itself is single-quoted, with `%` and `$` doubled for systemd. Measured in the KB note "ExecStart
/// arguments reach the program unchanged through an sh exec wrapper when written with systemd's
/// escapes".
fn render_task_exec_start(command: &[String]) -> String {
    let mut out = format!(r#"/bin/sh -c 'printf "%%s\n" "{RUN_MARKER} $$INVOCATION_ID"; exec "$$0" "$$@"'"#);
    for word in command {
        out.push(' ');
        out.push_str(&render_exec_arg(word));
    }
    out
}

/// The characters systemd reads as a prefix of `ExecStart=`'s first word, after unquoting, and
/// which quoting does not make literal (measured on 242 and 255).
const COMMAND_PREFIXES: [char; 5] = ['-', '@', ':', '+', '!'];

/// systemd's `Restart=` vocabulary.
fn render_restart(restart: Restart) -> &'static str {
    match restart {
        Restart::Never => "no",
        Restart::OnFailure => "on-failure",
        Restart::Always => "always",
    }
}

/// Render `ExecStart=`'s value: each word percent-doubled, then quoted per systemd's command-line
/// syntax wherever quoting is needed for readback (whitespace or a quoting-relevant character),
/// or wherever an unquoted word would take on a meaning of its own: a bare `;` (systemd's
/// command-line separator).
///
/// `command[0]` is an absolute path (`ServiceSpec::new`), so it never starts with a command-prefix
/// character (`-@:+!`), which quoting would not make literal.
///
/// Arguments are also dollar-doubled, since systemd substitutes `$FOO`/`${FOO}` and collapses
/// `$$` there. The executable path is not: systemd takes it as written. But it substitutes in
/// `argv[0]` too, so a path holding `$` is written as `@<path> <argv[0], dollar-doubled>`, where
/// `@` makes the next word `argv[0]` (measured on 242 and 255). A path without `$` is one word.
fn render_exec_start(command: &[String]) -> String {
    let (executable, arguments) = command.split_first().expect("a daemon has a command");
    debug_assert!(
        !executable.starts_with(COMMAND_PREFIXES),
        "`command[0]` {executable:?} is not an absolute path, which `ServiceSpec::new` guarantees"
    );
    let mut words = Vec::with_capacity(command.len() + 1);
    if executable.contains('$') {
        words.push(render_exec_word(&double_percent(&format!("@{executable}"))));
        words.push(render_exec_arg(executable));
    } else {
        words.push(render_exec_word(&double_percent(executable)));
    }
    words.extend(arguments.iter().map(|arg| render_exec_arg(arg)));
    words.join(" ")
}

fn render_exec_arg(arg: &str) -> String {
    render_exec_word(&double_percent(arg).replace('$', "$$"))
}

fn render_exec_word(escaped: &str) -> String {
    let is_bare_separator = escaped == ";";

    if needs_quoting(escaped) || is_bare_separator {
        quote(escaped)
    } else {
        escaped.to_string()
    }
}

/// Render one `Environment=`'s value: `KEY=value`, percent-doubled and
/// quoted as a single token so an embedded space cannot split it into two
/// assignments. `Environment=` is not systemd's command-line grammar, so
/// unlike `ExecStart=` it gets neither `$`-doubling nor the
/// separator treatment.
fn render_environment(key: &str, value: &str) -> String {
    let escaped = double_percent(&format!("{key}={value}"));
    if needs_quoting(&escaped) {
        quote(&escaped)
    } else {
        escaped
    }
}

/// Double every `%` in `s` — see the module doc for why.
fn double_percent(s: &str) -> String {
    s.replace('%', "%%")
}

/// A directive's value as systemd reads it verbatim: `Description=`, `WorkingDirectory=`,
/// `User=` and the `append:` log path take no unquoting, only specifiers.
///
/// A line ending in a backslash continues onto the next line, which would swallow the directive
/// after it (`User=` among them). systemd drops trailing whitespace after that check, so a
/// space behind an odd run of trailing backslashes ends the line and leaves the value intact; an
/// even run does not continue and is written as is.
fn raw_value(s: &str) -> String {
    let mut out = double_percent(s);
    if trailing_backslash_run(&out) % 2 == 1 {
        out.push(' ');
    }
    out
}

/// How many backslashes end `s`.
fn trailing_backslash_run(s: &str) -> usize {
    s.len() - s.trim_end_matches('\\').len()
}

/// Whether `s` needs systemd command-line quoting: it is empty, or
/// contains whitespace, an ASCII control character (only a quoted word can
/// carry it, as an escape) or a character that quoting itself is relevant
/// to (`"`, `'`, `\`).
fn needs_quoting(s: &str) -> bool {
    s.is_empty()
        || s.chars()
            .any(|c| c.is_whitespace() || c.is_ascii_control() || matches!(c, '"' | '\'' | '\\'))
}

/// Wrap `s` in double quotes per systemd's command-line syntax, escaping
/// embedded `"` and `\`, and an ASCII control character as `\n`, `\t` or
/// `\xNN` (a raw newline would end the unit's line).
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_ascii_control() => out.push_str(&format!("\\x{:02x}", c as u8)),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

// Extraction ==========================================================================================================

/// Extract the embedded metadata blob from a unit file's text.
///
/// Returns `Ok(None)` only when no `[X-Goetia]` section is present at
/// all — that is the "this is not (or no longer) ours" case, e.g. a
/// foreign unit or one Goetia has never touched. A section that *is*
/// present but does not decode into a valid blob is `Err`, and more than
/// one `[X-Goetia]` section is `Err` even if every copy decodes cleanly:
/// otherwise a forged extra section could let user-controlled unit text
/// choose which blob `extract` reads.
pub fn extract(unit_text: &str) -> Result<Option<Blob>, Error> {
    let lines: Vec<&str> = unit_text.lines().collect();

    let section_starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == "[X-Goetia]")
        .map(|(i, _)| i)
        .collect();

    let start = match section_starts.as_slice() {
        [] => return Ok(None),
        [only] => *only + 1,
        _ => {
            return Err(Error::blob(format!(
                "unit text has {n} [X-Goetia] sections, expected at most one",
                n = section_starts.len()
            )));
        }
    };

    let end = lines[start..]
        .iter()
        .position(|line| is_section_header(line))
        .map(|offset| start + offset)
        .unwrap_or(lines.len());

    // A second `Marker=` or `Spec=` line within the one section is
    // rejected for the same reason a second section is: a forged extra
    // line would otherwise let user-controlled text pick which of two
    // values `extract` trusts, silently overriding the genuine one.
    let mut marker: Option<&str> = None;
    let mut spec_value: Option<&str> = None;
    for line in &lines[start..end] {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("Marker=") {
            if marker.is_some() {
                return Err(Error::blob(
                    "[X-Goetia] section has more than one Marker= line".to_string(),
                ));
            }
            marker = Some(value);
        } else if let Some(value) = trimmed.strip_prefix("Spec=") {
            if spec_value.is_some() {
                return Err(Error::blob(
                    "[X-Goetia] section has more than one Spec= line".to_string(),
                ));
            }
            spec_value = Some(value);
        }
    }

    let marker = marker.ok_or_else(|| Error::blob("[X-Goetia] section has no Marker= line".to_string()))?;
    if marker != MARKER {
        return Err(Error::blob(format!(
            "[X-Goetia] section has unexpected Marker value `{marker}`"
        )));
    }

    let spec_value = spec_value.ok_or_else(|| Error::blob("[X-Goetia] section has no Spec= line".to_string()))?;

    // Our own generator never emits whitespace or a backslash inside
    // `Spec=`'s base64 value. Either one means the line was hand-edited
    // into a continuation (a trailing `\` before a following line, or a
    // literal embedded space) that this line-based reader does not
    // follow — reject rather than silently decode a truncated value.
    if spec_value.chars().any(|c| c.is_whitespace() || c == '\\') {
        return Err(Error::blob(
            "[X-Goetia] Spec= value contains whitespace or a line continuation".to_string(),
        ));
    }

    let blob = blob::decode(spec_value)?;
    Ok(Some(blob))
}

/// Whether `line`, trimmed, looks like an INI section header (`[...]`).
fn is_section_header(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('[') && trimmed.ends_with(']')
}

#[cfg(test)]
#[path = "generate_tests.rs"]
mod generate_tests;
