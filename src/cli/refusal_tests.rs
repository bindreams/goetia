use super::*;
use crate::error::Reason;

fn id() -> Id {
    Id::try_from("frpc").expect("`frpc` is a valid id")
}

/// `(exit code, stdout, stderr)` of reporting `outcome`.
fn reported(outcome: &Outcome, mood: Mood) -> (Option<ExitCode>, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = report(&id(), outcome, mood, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

#[skuld::test]
fn a_conflict_that_force_resolves_names_force() {
    let outcome = Outcome::Conflict {
        artifact_diff: "-a\n+b\n".to_string(),
        unclearable_recovery: None,
    };
    assert_eq!(
        reported(&outcome, Mood::Done),
        (
            Some(ExitCode::Conflict),
            "frpc: conflict (re-run with --force to overwrite)\n-a\n+b\n".to_string(),
            "error: frpc: conflict (re-run with --force to overwrite)\n".to_string()
        )
    );
    assert_eq!(
        reported(&outcome, Mood::Would),
        (
            Some(ExitCode::Conflict),
            "frpc: would conflict (hand-edited outside goetia; `install --force` would overwrite it)\n-a\n+b\n"
                .to_string(),
            "error: frpc: would conflict (hand-edited outside goetia; `install --force` would overwrite it)\n"
                .to_string()
        )
    );
}

#[skuld::test]
fn each_refusal_is_one_line_on_both_streams_and_its_own_exit_code() {
    let foreign = Outcome::RefuseForeign {
        recovery: "move it".to_string(),
    };
    let other = Outcome::RefuseOtherDaemon {
        installed: Id::try_from("other").unwrap(),
    };
    let unusable = Outcome::RefuseUnusable {
        reason: Reason::said("blob is damaged"),
        recovery: None,
    };
    let other_text = decide::other_daemon_refusal("other");
    for (outcome, mood, code, line) in [
        (
            &foreign,
            Mood::Done,
            ExitCode::Error,
            "frpc: refused: not a goetia-managed service. move it".to_string(),
        ),
        (
            &foreign,
            Mood::Would,
            ExitCode::Error,
            "frpc: would be refused: not a goetia-managed service. move it".to_string(),
        ),
        (
            &other,
            Mood::Done,
            ExitCode::Error,
            format!("frpc: refused: {other_text}"),
        ),
        (
            &other,
            Mood::Would,
            ExitCode::Error,
            format!("frpc: would be refused: {other_text}"),
        ),
        (
            &unusable,
            Mood::Done,
            ExitCode::Error,
            "frpc: refused: blob is damaged".to_string(),
        ),
        (
            &unusable,
            Mood::Would,
            ExitCode::Indeterminate,
            "frpc: would be refused: blob is damaged".to_string(),
        ),
    ] {
        assert_eq!(
            reported(outcome, mood),
            (Some(code), format!("{line}\n"), format!("error: {line}\n")),
            "{outcome:?} {mood:?}"
        );
    }
}

#[skuld::test]
fn a_recovery_is_appended_to_an_unusable_refusal_after_the_reason() {
    let outcome = Outcome::RefuseUnusable {
        reason: Reason::said("blob is damaged"),
        recovery: Some("uninstall it".to_string()),
    };
    assert_eq!(
        reported(&outcome, Mood::Done).1,
        "frpc: refused: blob is damaged. uninstall it\n"
    );
}

/// `recovery` and `reason` carry text from outside, and a line feed in one would start a line of
/// its own on either stream.
#[skuld::test]
fn what_a_refusal_quotes_is_escaped_so_the_line_stays_one_line() {
    let hostile = "a\nforged\u{1b}[31m\u{202e}";
    let escaped = r"a\u{a}forged\u{1b}[31m\u{202e}";
    let outcomes = [
        Outcome::Conflict {
            artifact_diff: String::new(),
            unclearable_recovery: Some(hostile.to_string()),
        },
        Outcome::RefuseForeign {
            recovery: hostile.to_string(),
        },
        Outcome::RefuseUnusable {
            reason: Reason::said(hostile),
            recovery: Some(hostile.to_string()),
        },
    ];
    for outcome in &outcomes {
        for mood in [Mood::Done, Mood::Would] {
            let (_, out, err) = reported(outcome, mood);
            for text in [&out, &err] {
                assert!(text.contains(escaped), "{outcome:?}: {text}");
                assert_eq!(text.lines().count(), 1, "{outcome:?}: {text}");
                assert!(!text.contains(['\u{1b}', '\u{202e}']), "{outcome:?}: {text}");
            }
        }
    }
}

#[skuld::test]
fn an_outcome_that_is_neither_a_conflict_nor_a_refusal_writes_nothing() {
    for outcome in [
        Outcome::Create,
        Outcome::UpToDate,
        Outcome::Update {
            spec_diff: "d".to_string(),
        },
        Outcome::Stale {
            from_version: "0.0.1".to_string(),
        },
    ] {
        assert_eq!(reported(&outcome, Mood::Done), (None, String::new(), String::new()));
    }
}
