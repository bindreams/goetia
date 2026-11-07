use super::*;
use crate::decide::{self, Outcome};
use crate::error::{Error, Reason};

fn id() -> Id {
    Id::try_from("frpc").expect("`frpc` is a valid id")
}

/// `(exit code, stdout, stderr)` of what `refuse` writes for `mood`.
fn reported(
    mood: Mood,
    refuse: impl FnOnce(&Lines, &mut Vec<u8>, &mut Vec<u8>) -> ExitCode,
) -> (ExitCode, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let id = id();
    let code = refuse(&Lines { id: &id, mood }, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

fn conflict(mood: Mood, recovery: Option<&str>, diff: &str) -> (ExitCode, String, String) {
    reported(mood, |lines, out, err| lines.conflict(diff, recovery, out, err))
}

#[skuld::test]
fn a_conflict_that_force_resolves_names_force() {
    assert_eq!(
        conflict(Mood::Done, None, "-a\n+b\n"),
        (
            ExitCode::Conflict,
            "frpc: conflict (re-run with --force to overwrite)\n-a\n+b\n".to_string(),
            "error: frpc: conflict (re-run with --force to overwrite)\n".to_string()
        )
    );
    assert_eq!(
        conflict(Mood::Would, None, "-a\n+b\n"),
        (
            ExitCode::Conflict,
            "frpc: would conflict (hand-edited outside goetia; `install --force` would overwrite it)\n-a\n+b\n"
                .to_string(),
            "error: frpc: would conflict (hand-edited outside goetia; `install --force` would overwrite it)\n"
                .to_string()
        )
    );
}

#[skuld::test]
fn each_refusal_is_one_line_on_both_streams_and_its_own_exit_code() {
    let other = Id::try_from("other").unwrap();
    let other_text = decide::other_daemon_refusal("other");
    for (mood, foreign_line, other_line, unusable_line, unusable_code) in [
        (
            Mood::Done,
            "frpc: refused: not a goetia-managed service. move it".to_string(),
            format!("frpc: refused: {other_text}"),
            "frpc: refused: blob is damaged",
            ExitCode::Error,
        ),
        (
            Mood::Would,
            "frpc: would be refused: not a goetia-managed service. move it".to_string(),
            format!("frpc: would be refused: {other_text}"),
            "frpc: would be refused: blob is damaged",
            ExitCode::Indeterminate,
        ),
    ] {
        let reasons = [
            (
                reported(mood, |lines, out, err| lines.foreign("move it", out, err)),
                ExitCode::Error,
                foreign_line,
            ),
            (
                reported(mood, |lines, out, err| lines.other_daemon(&other, out, err)),
                ExitCode::Error,
                other_line,
            ),
            (
                reported(mood, |lines, out, err| {
                    lines.unusable(&Reason::said("blob is damaged"), None, out, err)
                }),
                unusable_code,
                unusable_line.to_string(),
            ),
        ];
        for (reported, code, line) in reasons {
            assert_eq!(
                reported,
                (code, format!("{line}\n"), format!("error: {line}\n")),
                "{mood:?}"
            );
        }
    }
}

#[skuld::test]
fn a_recovery_is_appended_to_an_unusable_refusal_after_the_reason() {
    let (_, out, _) = reported(Mood::Done, |lines, out, err| {
        lines.unusable(&Reason::said("blob is damaged"), Some("uninstall it"), out, err)
    });
    assert_eq!(out, "frpc: refused: blob is damaged. uninstall it\n");
}

/// `recovery` and `reason` carry text from outside, and a line feed in one would start a line of
/// its own on either stream.
#[skuld::test]
fn what_a_refusal_quotes_is_escaped_so_the_line_stays_one_line() {
    let hostile = "a\nforged\u{1b}[31m\u{202e}";
    let escaped = r"a\u{a}forged\u{1b}[31m\u{202e}";
    for mood in [Mood::Done, Mood::Would] {
        let reports = [
            conflict(mood, Some(hostile), ""),
            reported(mood, |lines, out, err| lines.foreign(hostile, out, err)),
            reported(mood, |lines, out, err| {
                lines.unusable(&Reason::said(hostile), Some(hostile), out, err)
            }),
        ];
        for (n, (_, out, err)) in reports.iter().enumerate() {
            for text in [out, err] {
                assert!(text.contains(escaped), "{n} {mood:?}: {text}");
                assert_eq!(text.lines().count(), 1, "{n} {mood:?}: {text}");
                assert!(!text.contains(['\u{1b}', '\u{202e}']), "{n} {mood:?}: {text}");
            }
        }
    }
}

/// A reason holding a command's stderr prints as `list` prints it: the line strict, the stderr in
/// the layout, and goetia's recovery on a line of its own after it.
#[skuld::test]
fn an_unusable_refusal_writes_a_childs_stderr_in_the_layout() {
    let command = Error::Command {
        command: "systemctl show x".to_string(),
        status: crate::error::error_tests::exit_status(1),
        stderr: "first\nerror: forged\u{1b}[31m".to_string(),
    };
    let reason = Reason::failed(command);
    let shown = reason.to_string();
    assert!(shown.ends_with("first\n  error: forged\\u{1b}[31m"), "{shown:?}");
    for mood in [Mood::Done, Mood::Would] {
        let (_, out, err) = reported(mood, |lines, out, err| {
            lines.unusable(&reason, Some("uninstall it"), out, err)
        });
        // After the block, goetia's words continue under the prefix of the stream's own line.
        let line = |prefix: &str| format!("{prefix}{}: {shown}\n{prefix}. uninstall it\n", refused(mood));
        assert_eq!(out, line("frpc: "), "{mood:?}");
        assert_eq!(err, line("error: frpc: "), "{mood:?}");
    }
}

#[skuld::test]
fn only_a_conflict_or_a_refusal_is_a_refusal() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let id = id();
    let lines = Lines {
        id: &id,
        mood: Mood::Done,
    };
    let create = Outcome::Create;
    assert!(matches!(
        lines.refusal(&create, &mut out, &mut err),
        Err(NotRefused::Create)
    ));
    let up_to_date = Outcome::UpToDate;
    assert!(matches!(
        lines.refusal(&up_to_date, &mut out, &mut err),
        Err(NotRefused::UpToDate)
    ));
    let update = Outcome::Update {
        spec_diff: "d".to_string(),
    };
    assert!(matches!(
        lines.refusal(&update, &mut out, &mut err),
        Err(NotRefused::Update { spec_diff: "d" })
    ));
    let stale = Outcome::Stale {
        from_version: "0".to_string(),
    };
    assert!(matches!(
        lines.refusal(&stale, &mut out, &mut err),
        Err(NotRefused::Stale { from_version: "0" })
    ));
    assert!(out.is_empty() && err.is_empty());
    for (outcome, code) in [
        (
            Outcome::Conflict {
                artifact_diff: String::new(),
                unclearable_recovery: None,
            },
            ExitCode::Conflict,
        ),
        (
            Outcome::RefuseForeign {
                recovery: "move it".to_string(),
            },
            ExitCode::Error,
        ),
        (
            Outcome::RefuseOtherDaemon {
                installed: Id::try_from("other").unwrap(),
            },
            ExitCode::Error,
        ),
        (
            Outcome::RefuseUnusable {
                reason: Reason::said("damaged"),
                recovery: None,
            },
            ExitCode::Error,
        ),
    ] {
        assert!(
            matches!(lines.refusal(&outcome, &mut out, &mut err), Ok(c) if c == code),
            "{outcome:?}"
        );
    }
}
