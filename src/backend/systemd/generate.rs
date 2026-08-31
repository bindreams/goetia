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
//! further doubles `$` (systemd performs `$FOO`/`${FOO}` environment
//! substitution there), quotes a bare `;` argument (systemd's
//! command-line separator), and quotes `command[0]` if it would otherwise
//! start with one of systemd's command-prefix characters (`-@:+!`).

use crate::backend::Identity;
use crate::blob::{self, Blob, MARKER};
use crate::error::Error;
use crate::spec::{DaemonSpec, Restart};

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
pub fn unit(spec: &DaemonSpec, id: &Identity) -> String {
    let mut out = String::new();

    out.push_str("[Unit]\n");
    out.push_str(&format!("Description={}\n", double_percent(&spec.name)));
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
    out.push_str("Type=simple\n");
    out.push_str(&format!("ExecStart={}\n", render_exec_start(&spec.command)));
    if let Some(cwd) = &spec.cwd {
        out.push_str(&format!(
            "WorkingDirectory={}\n",
            double_percent(&cwd.display().to_string())
        ));
    }
    for (key, value) in &spec.env {
        out.push_str(&format!("Environment={}\n", render_environment(key, value)));
    }
    out.push_str(&format!("User={}\n", double_percent(&id.user)));
    out.push_str(&format!("Restart={}\n", render_restart(spec.restart)));
    if let Some(delay) = spec.restart_delay {
        out.push_str(&format!("RestartSec={}\n", delay.as_secs_f64()));
    }
    if let Some(logs) = &spec.logs {
        let target = format!("append:{}", double_percent(&logs.display().to_string()));
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

    out
}

/// systemd's `Restart=` vocabulary.
fn render_restart(restart: Restart) -> &'static str {
    match restart {
        Restart::Never => "no",
        Restart::OnFailure => "on-failure",
        Restart::Always => "always",
    }
}

/// Render `ExecStart=`'s value: each argument percent- and dollar-doubled,
/// then quoted per systemd's command-line syntax wherever quoting is
/// needed for readback (whitespace or a quoting-relevant character), or
/// wherever an unquoted word would take on a meaning of its own — a bare
/// `;` (systemd's command-line separator) or, for `command[0]` only, a
/// leading `-`/`@`/`:`/`+`/`!` (systemd's command-prefix characters,
/// e.g. `+` for elevated-privilege execution — a plain executable name
/// that happens to start with one must not be reinterpreted as that
/// directive).
fn render_exec_start(command: &[String]) -> String {
    command
        .iter()
        .enumerate()
        .map(|(index, arg)| render_exec_arg(arg, index == 0))
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_exec_arg(arg: &str, is_executable: bool) -> String {
    // `ExecStart=` is systemd's "command line" grammar: on top of `%`
    // specifiers, it performs `$FOO`/`${FOO}` environment-variable
    // substitution, so `$` needs doubling here exactly as `%` does.
    let escaped = double_percent(arg).replace('$', "$$");

    let is_bare_separator = escaped == ";";
    let is_command_prefix = is_executable && escaped.starts_with(['-', '@', ':', '+', '!']);

    if needs_quoting(&escaped) || is_bare_separator || is_command_prefix {
        quote(&escaped)
    } else {
        escaped
    }
}

/// Render one `Environment=`'s value: `KEY=value`, percent-doubled and
/// quoted as a single token so an embedded space cannot split it into two
/// assignments. `Environment=` is not systemd's command-line grammar, so
/// unlike `ExecStart=` it gets neither `$`-doubling nor the
/// separator/prefix-character treatment.
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
            return Err(Error::Blob(format!(
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
                return Err(Error::Blob(
                    "[X-Goetia] section has more than one Marker= line".to_string(),
                ));
            }
            marker = Some(value);
        } else if let Some(value) = trimmed.strip_prefix("Spec=") {
            if spec_value.is_some() {
                return Err(Error::Blob(
                    "[X-Goetia] section has more than one Spec= line".to_string(),
                ));
            }
            spec_value = Some(value);
        }
    }

    let marker = marker.ok_or_else(|| Error::Blob("[X-Goetia] section has no Marker= line".to_string()))?;
    if marker != MARKER {
        return Err(Error::Blob(format!(
            "[X-Goetia] section has unexpected Marker value `{marker}`"
        )));
    }

    let spec_value = spec_value.ok_or_else(|| Error::Blob("[X-Goetia] section has no Spec= line".to_string()))?;

    // Our own generator never emits whitespace or a backslash inside
    // `Spec=`'s base64 value. Either one means the line was hand-edited
    // into a continuation (a trailing `\` before a following line, or a
    // literal embedded space) that this line-based reader does not
    // follow — reject rather than silently decode a truncated value.
    if spec_value.chars().any(|c| c.is_whitespace() || c == '\\') {
        return Err(Error::Blob(
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
