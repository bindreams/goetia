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
//! substitution there, but not in the executable path; it does in argv[0], so a path holding `$`
//! is written `@<path> <argv[0]>`), and quotes a bare `;` argument (systemd's
//! command-line separator).
//!
//! Backslashes are never refused: a quoted word doubles each, and a raw value ending in an odd
//! run of them gets a trailing space (see [`raw_value`]). The values no encoding can carry are
//! refused by [`unit`]: see [`refuse_unrepresentable`].

use crate::backend::Identity;
use crate::blob::{self, Blob, MARKER};
use crate::error::Error;
use crate::spec::{DaemonSpec, Restart, is_valid_name};

// Generation ==========================================================================================================

/// Render `spec` as a systemd unit file for the already-resolved account
/// `id`.
///
/// The `[Install]` section is emitted so `systemctl enable` is possible
/// later, but generation never consults enablement state — see the
/// crate-level design notes on why boot-enablement is excluded from every
/// drift comparison.
///
/// `unit(&extract(&unit(spec, id)).unwrap().unwrap().spec, id)` must be
/// byte-identical to `unit(spec, id)`.
///
/// Fails with [`Error::Invalid`] only for a value systemd cannot carry; see [`refuse_unrepresentable`].
///
/// Emits `Type=exec` (requires systemd 240+, 2018): `systemd.service(5)` recommends it for
/// long-running services "as it ensures that process setup errors (e.g. errors such as a missing
/// service executable, or missing user) are properly tracked", which `Type=simple` does not —
/// `systemctl start` there can report success for a unit that immediately goes `failed`. The
/// accepted cost is that `exec` makes systemd block on the NSS lookup `User=` triggers before
/// reporting back; every generated unit carries `User=`, so this applies to all of them, and
/// `--timeout` bounds and reports it.
///
/// Changing what this function emits after a release ships requires bumping `version` in the same
/// commit (see `blob::encode`), or every already-installed service using the old bytes reports as
/// hand-edited: `decide`'s version-mismatch check (`src/decide.rs`) is what tells "stale, regenerate
/// silently" from "conflict, demand `--force`" apart, and it runs before either `desired` or
/// `regenerated` is even compared.
pub fn unit(spec: &DaemonSpec, id: &Identity) -> Result<String, Error> {
    refuse_unrepresentable(spec, id)?;
    let mut out = String::new();

    out.push_str("[Unit]\n");
    out.push_str(&format!("Description={}\n", raw_value(&spec.name)));
    if spec.restart != Restart::Never {
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
    out.push_str(&format!("ExecStart={}\n", render_exec_start(&spec.command)));
    if let Some(cwd) = &spec.cwd {
        out.push_str(&format!("WorkingDirectory={}\n", raw_value(&cwd.display().to_string())));
    }
    for (key, value) in &spec.env {
        out.push_str(&format!("Environment={}\n", render_environment(key, value)));
    }
    out.push_str(&format!("User={}\n", raw_value(&id.user)));
    out.push_str(&format!("Restart={}\n", render_restart(spec.restart)));
    if let Some(delay) = spec.restart_delay {
        out.push_str(&format!("RestartSec={}\n", delay.as_secs_f64()));
    }
    if let Some(logs) = &spec.logs {
        let target = format!("append:{}", raw_value(&logs.display().to_string()));
        out.push_str(&format!("StandardOutput={target}\n"));
        out.push_str(&format!("StandardError={target}\n"));
    }
    out.push('\n');

    out.push_str("[Install]\n");
    out.push_str("WantedBy=multi-user.target\n");
    out.push('\n');

    out.push_str("[X-Goetia]\n");
    out.push_str(&format!("Marker={MARKER}\n"));
    out.push_str(&format!("Schema={}\n", blob::SCHEMA));
    out.push_str(&format!("Version={}\n", crate::version()));
    out.push_str(&format!("Spec={}\n", blob::encode(spec)));

    Ok(out)
}

/// Refuse what no encoding carries through systemd, with the measured reason.
///
/// - The executable path of `ExecStart=` (after unquoting and specifiers) holding a backslash,
///   `"` or `'`: systemd refuses the whole unit with "Executable name contains special
///   characters", however the word was quoted. Later arguments take all three.
/// - An executable path ending in a space: systemd 255 keeps the path but the service exits with
///   203/EXEC (242 runs it; both are refused). Interior spaces, other trailing whitespace and
///   later arguments are fine.
/// - An `env` key outside `[A-Za-z_][A-Za-z0-9_]*`: systemd logs "Invalid environment
///   assignment, ignoring" and drops it, and the unit still loads and starts.
/// - A raw directive (`Description=`, `User=`, `WorkingDirectory=`, the `append:` path) with
///   leading or trailing whitespace: systemd strips it, and no quote, C escape or line
///   continuation keeps it. `cwd` and `logs` are absolute, so only their end can be affected.
fn refuse_unrepresentable(spec: &DaemonSpec, id: &Identity) -> Result<(), Error> {
    let invalid = |message: String| Error::Invalid {
        daemon: spec.id.to_string(),
        message,
        source: None,
    };

    let executable = &spec.command[0];
    if executable.contains(['\\', '"', '\'']) {
        return Err(invalid(format!(
            "the executable path {executable:?} (`command[0]`) \
             holds a backslash, double quote or apostrophe, which systemd refuses to load in an \
             executable path"
        )));
    }
    if executable.ends_with(' ') {
        return Err(invalid(format!(
            "the executable path {executable:?} (`command[0]`) \
             ends in whitespace, which systemd does not run (the service exits with 203/EXEC)"
        )));
    }
    if let Some(key) = spec.env.keys().find(|key| !is_valid_name(key)) {
        return Err(invalid(format!(
            "`env` key {key:?} is not a name systemd accepts in Environment=: it must match \
             [A-Za-z_][A-Za-z0-9_]*, and systemd drops any other assignment with only a log line, so the \
             daemon would never see it"
        )));
    }

    let cwd = spec.cwd.as_ref().map(|p| p.display().to_string());
    let logs = spec.logs.as_ref().map(|p| p.display().to_string());
    let raw_values = [
        ("Description=", Some(spec.name.as_str())),
        ("User=", Some(id.user.as_str())),
        ("WorkingDirectory=", cwd.as_deref()),
        ("StandardOutput=/StandardError= append:", logs.as_deref()),
    ];
    for (directive, value) in raw_values {
        let Some(value) = value else { continue };
        if value.starts_with(STRIPPED_BY_SYSTEMD) || value.ends_with(STRIPPED_BY_SYSTEMD) {
            return Err(invalid(format!(
                "{directive} value {value:?} starts or ends with whitespace, which systemd strips from \
                 this directive; no quoting or escape keeps it"
            )));
        }
    }
    Ok(())
}

/// The characters systemd reads as a prefix of `ExecStart=`'s first word, after unquoting, and
/// which quoting does not make literal (measured on 242 and 255).
const COMMAND_PREFIXES: [char; 5] = ['-', '@', ':', '+', '!'];

/// The characters systemd's line reader strips from both ends of a raw value: space, tab, LF, CR.
/// VT, FF and Unicode whitespace survive.
const STRIPPED_BY_SYSTEMD: [char; 4] = [' ', '\t', '\n', '\r'];

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
/// `command[0]` never starts with a command-prefix character (`-@:+!`), which quoting would not
/// make literal: `DaemonSpec::new` guarantees it is an absolute path, and a Unix one starts with `/`.
///
/// Arguments are also dollar-doubled, since systemd substitutes `$FOO`/`${FOO}` and collapses
/// `$$` there. The executable path is not: systemd takes it as written. But it substitutes in
/// argv[0] too, so a path holding `$` is written as `@<path> <argv[0], dollar-doubled>`, where
/// `@` makes the next word argv[0] (measured on 242 and 255). A path without `$` is one word.
fn render_exec_start(command: &[String]) -> String {
    let (executable, arguments) = command.split_first().expect("a daemon has a command");
    debug_assert!(
        !executable.starts_with(COMMAND_PREFIXES),
        "`command[0]` {executable:?} is not an absolute path, which `DaemonSpec::new` guarantees"
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
    let trailing = out.len() - out.trim_end_matches('\\').len();
    if trailing % 2 == 1 {
        out.push(' ');
    }
    out
}

/// Whether `s` needs systemd command-line quoting: it is empty, or
/// contains whitespace or a character that quoting itself is relevant to
/// (`"`, `'`, `\`).
fn needs_quoting(s: &str) -> bool {
    s.is_empty() || s.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\'))
}

/// Wrap `s` in double quotes per systemd's command-line syntax, escaping
/// embedded `"` and `\`.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
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
