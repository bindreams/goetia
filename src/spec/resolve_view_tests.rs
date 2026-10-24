//! The class test for step 5a: every check and advisory reads, per field, the
//! value the install for that backend would read, and one field's failure
//! never changes another field's verdict.
//!
//! The fields the checks read are exactly those `Backend::reject_merged` and
//! `Backend::warn` (and the advisories they call) read off a `ShapedSpec`:
//! `restart` (the `scm` refusal, the over-long-delay advisory), `restart-delay`
//! (launchd rounding, the over-long-delay advisory), `type` (both `scm`
//! advisories), and `cwd` and `logs` (presence only, for the managed
//! advisory). A field added to those readers must be added to `Field` and to a
//! scenario below.

use std::path::Path;

use super::*;

fn parse_manifest(yaml: &str) -> RawManifest {
    yaml_serde::from_str(yaml).expect("fixture yaml should parse")
}

/// A fresh directory holding `.env` when `env` is given.
fn env_dir(env: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir should be creatable");
    if let Some(env) = env {
        std::fs::write(dir.path().join(".env"), env).expect("fixture .env should be writable");
    }
    dir
}

fn run(yaml: &str, dir: &Path, native: Backend) -> Result<Vec<Warning>, String> {
    resolve_as(parse_manifest(yaml), dir, Some(native))
        .map(|(_specs, warnings)| warnings)
        .map_err(|e| e.to_string())
}

// Fields ==============================================================================================================

/// Every field a manifest can write, bar `id`. The first five are the ones the
/// checks read; the rest are the unrelated fields that can fail.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Field {
    Restart,
    RestartDelay,
    Type,
    Cwd,
    Logs,
    Command,
    Name,
    User,
    Env,
}

const FIELDS: [Field; 9] = [
    Field::Restart,
    Field::RestartDelay,
    Field::Type,
    Field::Cwd,
    Field::Logs,
    Field::Command,
    Field::Name,
    Field::User,
    Field::Env,
];

/// An XML noncharacter: `resolve_shape` rejects it in every field below, and
/// a `.env` value may carry it, so it makes a field "invalid after
/// substitution" without being invalid as authored.
const UNEMITTABLE: &str = "x\u{FFFE}";

impl Field {
    /// One YAML line setting this field to `value` (always a quoted scalar).
    fn line(self, value: &str) -> String {
        match self {
            Field::Restart => format!("restart: \"{value}\""),
            Field::RestartDelay => format!("restart-delay: \"{value}\""),
            Field::Type => format!("type: \"{value}\""),
            Field::Cwd => format!("cwd: \"{value}\""),
            Field::Logs => format!("logs: \"{value}\""),
            Field::Command => format!("command: [\"{value}\"]"),
            Field::Name => format!("name: \"{value}\""),
            Field::User => format!("user: {{name: \"{value}\"}}"),
            Field::Env => format!("env: {{K: \"{value}\"}}"),
        }
    }

    /// A valid literal that no check reads anything into.
    fn valid(self) -> &'static str {
        match self {
            Field::Restart => "on-failure",
            Field::RestartDelay => "5s",
            Field::Type => "simple",
            Field::Cwd => "/srv/u",
            Field::Logs => "/srv/l",
            Field::Command => "/bin/svc",
            Field::Name => "u",
            Field::User => "nobody",
            Field::Env => "v",
        }
    }

    /// A `.env` value that makes this field fail its own shape check once
    /// substituted.
    fn invalid(self) -> &'static str {
        match self {
            Field::Restart => "sometimes",
            Field::RestartDelay => "garbage",
            Field::Type => "bogus",
            Field::Cwd | Field::Logs | Field::Command | Field::Name | Field::User | Field::Env => UNEMITTABLE,
        }
    }
}

// Manifests ===========================================================================================================

/// A one-daemon manifest: `base` lines (a `command` is added unless `base`
/// sets one) and one `backend-specific.<native>` override.
fn manifest(base: &[(Field, String)], native: Backend, overrides: &[(Field, String)]) -> String {
    let mut yaml = String::from("daemons:\n  svc:\n");
    if base.iter().all(|(field, _)| *field != Field::Command) {
        yaml.push_str("    command: [\"/bin/svc\"]\n");
    }
    for (field, value) in base {
        yaml.push_str(&format!("    {}\n", field.line(value)));
    }
    if !overrides.is_empty() {
        yaml.push_str("    backend-specific:\n");
        yaml.push_str(&format!("      {native}:\n"));
        for (field, value) in overrides {
            yaml.push_str(&format!("        {}\n", field.line(value)));
        }
    }
    yaml
}

// Scenarios ===========================================================================================================

enum Expect {
    /// Every host refuses with the `restart: always` message.
    Refused,
    /// Every host succeeds with a warning containing this text.
    Warns(&'static str),
}

/// One thing a check says about one templated field, with every other field
/// it reads written as a literal.
struct Scenario {
    name: &'static str,
    /// The templated field under test, and the value `.env` gives it.
    field: Field,
    value: &'static str,
    /// The other fields the same check reads, as literals.
    literals: &'static [(Field, &'static str)],
    /// A valid literal for `field`, for the native override that keeps the
    /// native pass from needing `.env`.
    native: &'static str,
    expect: Expect,
}

const SCENARIOS: [Scenario; 8] = [
    Scenario {
        name: "restart: always refused by scm",
        field: Field::Restart,
        value: "always",
        literals: &[],
        native: "on-failure",
        expect: Expect::Refused,
    },
    Scenario {
        name: "launchd rounds a sub-second restart-delay",
        field: Field::RestartDelay,
        value: "500ms",
        literals: &[],
        native: "5s",
        expect: Expect::Warns("ThrottleInterval"),
    },
    Scenario {
        name: "type: managed warns about cwd (type templated)",
        field: Field::Type,
        value: "managed",
        literals: &[(Field::Cwd, "/srv/c")],
        native: "simple",
        expect: Expect::Warns("type: managed"),
    },
    Scenario {
        name: "type: managed warns about cwd (cwd templated)",
        field: Field::Cwd,
        value: "/srv/d",
        literals: &[(Field::Type, "managed")],
        native: "/srv/n",
        expect: Expect::Warns("type: managed"),
    },
    Scenario {
        name: "type: managed warns about logs (logs templated)",
        field: Field::Logs,
        value: "/srv/e",
        literals: &[(Field::Type, "managed")],
        native: "/srv/n",
        expect: Expect::Warns("type: managed"),
    },
    Scenario {
        name: "scm clamps an over-long delay (restart templated)",
        field: Field::Restart,
        value: "on-failure",
        literals: &[(Field::Type, "managed"), (Field::RestartDelay, "5000000s")],
        native: "never",
        expect: Expect::Warns("exceeds"),
    },
    Scenario {
        name: "scm clamps an over-long delay (restart-delay templated)",
        field: Field::RestartDelay,
        value: "5000000s",
        literals: &[(Field::Type, "managed"), (Field::Restart, "on-failure")],
        native: "5s",
        expect: Expect::Warns("exceeds"),
    },
    Scenario {
        name: "scm clamps an over-long delay (type templated)",
        field: Field::Type,
        value: "managed",
        literals: &[(Field::Restart, "on-failure"), (Field::RestartDelay, "5000000s")],
        native: "simple",
        expect: Expect::Warns("exceeds"),
    },
];

impl Scenario {
    /// Whether the scenario's verdict depends on `field`. Under `type: managed`
    /// that includes `cwd` and `logs`, whose presence the managed advisory reads.
    fn reads(&self, field: Field) -> bool {
        let managed =
            (self.field == Field::Type && self.value == "managed") || self.literals.contains(&(Field::Type, "managed"));
        let presence = managed && matches!(field, Field::Cwd | Field::Logs);
        presence || self.field == field || self.literals.iter().any(|(f, _)| *f == field)
    }

    fn literal_lines(&self) -> Vec<(Field, String)> {
        self.literals.iter().map(|(f, v)| (*f, (*v).to_string())).collect()
    }

    fn check_reference(&self, reference: &Result<Vec<Warning>, String>, label: &str) {
        match (&self.expect, reference) {
            (Expect::Refused, Err(message)) => {
                assert!(
                    message.contains("`restart: always` cannot be honoured"),
                    "{label}: {message}"
                );
            }
            (Expect::Warns(text), Ok(warnings)) => {
                assert!(
                    warnings.iter().any(|w| w.message.contains(text)),
                    "{label}: the reference must carry a `{text}` advisory: {warnings:?}"
                );
            }
            (_, other) => panic!("{label}: the reference does not say what the scenario pins: {other:?}"),
        }
    }
}

// The class ===========================================================================================================

#[skuld::test]
fn an_unrelated_field_that_fails_never_changes_the_verdict_on_a_field_the_checks_read() {
    for scenario in &SCENARIOS {
        for native in Backend::ALL {
            for unrelated in FIELDS.into_iter().filter(|f| !scenario.reads(*f)) {
                for (mode, template, env_bad) in [
                    ("unsubstitutable", "${NOPE}", false),
                    ("invalid once substituted", "${BAD}", true),
                ] {
                    let label = format!("{} / native {native} / unrelated {unrelated:?} {mode}", scenario.name);
                    let env = format!(
                        "V={}\n{}",
                        scenario.value,
                        if env_bad {
                            format!("BAD={}\n", unrelated.invalid())
                        } else {
                            String::new()
                        }
                    );
                    let dir = env_dir(Some(&env));

                    let mut base = scenario.literal_lines();
                    base.insert(0, (scenario.field, "${V}".to_string()));
                    let overrides = [(unrelated, unrelated.valid().to_string())];

                    let mut failing = base.clone();
                    failing.push((unrelated, template.to_string()));
                    let mut valid = base;
                    valid.push((unrelated, unrelated.valid().to_string()));

                    let reference = run(&manifest(&valid, native, &overrides), dir.path(), native);
                    scenario.check_reference(&reference, &label);
                    let got = run(&manifest(&failing, native, &overrides), dir.path(), native);
                    assert_eq!(got, reference, "{label}");
                }
            }
        }
    }
}

#[skuld::test]
fn a_field_under_test_that_cannot_be_determined_is_unjudged() {
    // The native override replaces the templated field, so the install needs
    // no value for it; the other backends inherit a base value that is
    // undefined, invalid once substituted, or a `${V:-default}` whose default
    // is exactly the value that would trip a check while `.env` is unreadable.
    // Nothing may be judged on it: the verdict is that of the manifest with the
    // field not written at all.
    for scenario in &SCENARIOS {
        for native in Backend::ALL {
            for (mode, template, env) in [
                ("undefined", "${NOPE}".to_string(), Some("X=1\n".to_string())),
                (
                    "invalid once substituted",
                    "${BAD}".to_string(),
                    Some(format!("BAD={}\n", scenario.field.invalid())),
                ),
                ("unreadable .env", format!("${{V:-{}}}", scenario.value), None),
            ] {
                let label = format!("{} / native {native} / {mode}", scenario.name);
                let dir = env_dir(env.as_deref());
                if env.is_none() {
                    // `.env` is a directory, so `Vars::load` fails.
                    std::fs::create_dir(dir.path().join(".env")).expect("fixture directory should be creatable");
                }

                let overrides = [(scenario.field, scenario.native.to_string())];
                let mut templated = scenario.literal_lines();
                templated.insert(0, (scenario.field, template));

                let got = run(&manifest(&templated, native, &overrides), dir.path(), native);
                let expected = run(
                    &manifest(&scenario.literal_lines(), native, &overrides),
                    dir.path(),
                    native,
                );
                assert!(expected.is_ok(), "{label}: {expected:?}");
                assert_eq!(got, expected, "{label}");
            }
        }
    }
}

#[skuld::test]
fn a_templated_field_in_a_non_native_override_is_unjudged() {
    // A backend this host cannot install never has its override substituted,
    // so the `$` stays deferred: the verdict is that of the unwritten field.
    for scenario in &SCENARIOS {
        for native in Backend::ALL {
            for other in Backend::ALL.into_iter().filter(|b| *b != native) {
                let label = format!("{} / native {native} / templated in {other}", scenario.name);
                let dir = env_dir(Some(&format!("V={}\n", scenario.value)));

                let templated = [(scenario.field, "${V}".to_string())];
                let got = run(
                    &manifest(&scenario.literal_lines(), other, &templated),
                    dir.path(),
                    native,
                );
                let expected = run(&manifest(&scenario.literal_lines(), other, &[]), dir.path(), native);
                assert!(expected.is_ok(), "{label}: {expected:?}");
                assert_eq!(got, expected, "{label}");
            }
        }
    }
}

// Named cases =========================================================================================================

fn assert_refused(result: &Result<Vec<Warning>, String>, label: &str) {
    let message = result.as_ref().expect_err(label);
    assert!(
        message.contains("`restart: always` cannot be honoured"),
        "{label}: {message}"
    );
}

#[skuld::test]
fn a_restart_delay_that_substitutes_to_garbage_does_not_hide_a_templated_always() {
    let yaml = "daemons:\n  svc:\n    command: [/bin/svc]\n    restart: ${R}\n    restart-delay: ${D}\n    \
                backend-specific:\n      systemd: { restart-delay: 5s }\n      launchd: { restart-delay: 5s }\n";
    let dir = env_dir(Some("R=always\nD=garbage\n"));
    for native in [Backend::Systemd, Backend::Launchd] {
        assert_refused(&run(yaml, dir.path(), native), &format!("native {native}"));
    }
}

#[skuld::test]
fn an_empty_cwd_that_substitutes_does_not_hide_a_templated_always() {
    let yaml = "daemons:\n  svc:\n    command: [/bin/svc]\n    restart: ${R}\n    cwd: ${C}\n    \
                backend-specific:\n      systemd: { cwd: /srv/s }\n      launchd: { cwd: /srv/s }\n";
    let dir = env_dir(Some("R=always\nC=\n"));
    for native in [Backend::Systemd, Backend::Launchd] {
        assert_refused(&run(yaml, dir.path(), native), &format!("native {native}"));
    }
}

#[skuld::test]
fn an_unknown_type_that_substitutes_does_not_hide_an_advisory() {
    let templated = "daemons:\n  svc:\n    command: [/bin/svc]\n    restart-delay: ${D}\n    type: ${K}\n    \
                     backend-specific:\n      systemd: { type: simple }\n      scm: { type: simple }\n";
    let literal = "daemons:\n  svc:\n    command: [/bin/svc]\n    restart-delay: 500ms\n    type: simple\n";
    let dir = env_dir(Some("D=500ms\nK=bogus\n"));
    let from_env = run(templated, dir.path(), Backend::Systemd).expect("resolves");
    let from_literal = run(literal, dir.path(), Backend::Systemd).expect("resolves");
    assert!(
        from_literal.iter().any(|w| w.message.contains("ThrottleInterval")),
        "{from_literal:?}"
    );
    assert_eq!(from_env, from_literal);
}

#[skuld::test]
fn a_default_is_not_the_resolved_value_when_env_cannot_be_read() {
    let yaml = "daemons:\n  svc:\n    command: [/bin/svc]\n    restart: ${R:-always}\n    \
                backend-specific:\n      systemd: { restart: on-failure }\n      launchd: { restart: on-failure }\n";
    let dir = env_dir(Some("this is not valid\n"));
    for native in [Backend::Systemd, Backend::Launchd] {
        run(yaml, dir.path(), native).unwrap_or_else(|e| panic!("native {native}: judged on a guess: {e}"));
    }
}
