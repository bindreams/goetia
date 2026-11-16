use std::time::Duration;

use super::*;
use crate::spec::raw::RawSpec;

fn raw(yaml: &str) -> Result<Option<RawStopAction>, yaml_serde::Error> {
    RawSpec::parse(&format!("id: d\ncommand: [/x]\n{yaml}")).map(|spec| spec.stop_action)
}

fn id() -> Id {
    Id::try_from("d").unwrap()
}

// The signal ==========================================================================================================

#[skuld::test]
fn the_portable_and_windows_names_parse() {
    for (text, signal) in [
        ("terminate", StopSignal::Terminate),
        ("interrupt", StopSignal::Interrupt),
        ("quit", StopSignal::Quit),
        ("kill", StopSignal::Kill),
        ("ctrl-c", StopSignal::CtrlC),
        ("ctrl-break", StopSignal::CtrlBreak),
        ("wm-close", StopSignal::WmClose),
    ] {
        assert_eq!(StopSignal::parse(text), Ok(signal), "{text}");
        assert_eq!(signal.to_string(), text);
    }
}

#[skuld::test]
fn a_native_signal_reads_in_any_case_and_is_written_as_systemd_names_it() {
    for (text, written) in [
        ("sigusr1", "SIGUSR1"),
        ("SIGUSR1", "SIGUSR1"),
        ("SigHup", "SIGHUP"),
        ("sigrtmin", "SIGRTMIN"),
        ("SIGRTMIN+0", "SIGRTMIN"),
        ("sigrtmin+15", "SIGRTMIN+15"),
        ("sigrtmin+30", "SIGRTMIN+30"),
        ("SIGRTMAX", "SIGRTMAX"),
        ("sigrtmax-14", "SIGRTMAX-14"),
        ("sigrtmax-30", "SIGRTMAX-30"),
    ] {
        let signal = StopSignal::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert!(matches!(signal, StopSignal::Native(_)), "{text}");
        assert_eq!(signal.to_string(), written, "{text}");
        assert_eq!(StopSignal::parse(written), Ok(signal), "{text} round trip");
    }
}

#[skuld::test]
fn every_native_name_systemd_has_parses() {
    for name in NATIVE_SIGNALS {
        assert_eq!(StopSignal::parse(name).unwrap().to_string(), name);
    }
}

#[skuld::test]
fn a_typo_is_an_error_naming_the_choices_and_never_a_signal() {
    for text in [
        "terminat",
        "Terminate",
        "usr1",
        "sigterrm",
        "SIG",
        "sigrtmin+31",
        "sigrtmax-31",
        "sigrtmin-1",
        "sigrtmax+1",
        "sigrtmin+",
        "sigrtmin+x",
        "sigrtmin+999",
        "",
    ] {
        let message = StopSignal::parse(text).expect_err(text);
        assert!(message.contains(&format!("`{text}`")), "{text}: {message}");
        assert!(
            message.contains("`terminate`") && message.contains("sigusr1"),
            "{text}: {message}"
        );
    }
}

#[skuld::test]
fn each_platform_refuses_the_signals_it_cannot_send() {
    use StopSignal::*;
    let native = StopSignal::parse("sigusr1").unwrap();
    let all = [Terminate, Interrupt, Quit, Kill, CtrlC, CtrlBreak, WmClose, native];
    let accepted = |backend| -> Vec<StopSignal> { all.into_iter().filter(|s| s.defect(backend).is_none()).collect() };

    assert_eq!(
        accepted(BackendKind::Systemd),
        [Terminate, Interrupt, Quit, Kill, native]
    );
    assert_eq!(accepted(BackendKind::Launchd), [Terminate, Kill]);
    assert_eq!(
        accepted(BackendKind::Scm),
        [Terminate, Interrupt, Quit, Kill, CtrlC, CtrlBreak, WmClose]
    );

    let refusal = Quit.defect(BackendKind::Launchd).unwrap();
    assert!(
        refusal.contains("`send-signal: quit`") && refusal.contains("launchd"),
        "{refusal}"
    );
}

// The timeout =========================================================================================================

#[skuld::test]
fn a_timeout_is_whole_seconds_from_one_second_to_a_day() {
    for (text, seconds) in [
        ("1s", 1),
        ("30s", 30),
        ("1m 30s", 90),
        ("2m", 120),
        ("24h", 86_400),
        ("1day", 86_400),
    ] {
        assert_eq!(
            parse_stop_timeout(&id(), text).unwrap(),
            Duration::from_secs(seconds),
            "{text}"
        );
    }
    for text in ["0s", "0", "500ms", "1.5s", "1s 1ms", "25h", "2days", "30", "soon", ""] {
        let message = parse_stop_timeout(&id(), text).expect_err(text).to_string();
        assert!(message.contains("stop-action.timeout"), "{text}: {message}");
    }
}

#[skuld::test]
fn a_timeout_that_is_not_a_duration_shows_an_example_and_names_null() {
    let message = parse_stop_timeout(&id(), "soon").unwrap_err().to_string();
    assert!(
        message.contains("`soon`") && message.contains("`30s`") && message.contains("`null`"),
        "{message}"
    );
}

#[skuld::test]
fn a_timeout_outside_the_range_says_so() {
    assert!(
        parse_stop_timeout(&id(), "25h")
            .unwrap_err()
            .to_string()
            .contains("1s to 24h")
    );
    assert!(
        parse_stop_timeout(&id(), "500ms")
            .unwrap_err()
            .to_string()
            .contains("whole number of seconds")
    );
}

// A resolved action ===================================================================================================

#[skuld::test]
fn the_default_is_terminate_with_thirty_seconds() {
    assert_eq!(
        StopAction::default(),
        StopAction {
            send_signal: StopSignal::Terminate,
            timeout: Some(Duration::from_secs(30)),
        }
    );
}

#[skuld::test]
fn kill_defaults_to_no_timeout_and_every_other_signal_to_thirty_seconds() {
    assert_eq!(StopAction::default_timeout(StopSignal::Kill), None);
    for signal in [
        "terminate",
        "interrupt",
        "quit",
        "ctrl-c",
        "ctrl-break",
        "wm-close",
        "sigusr1",
    ] {
        let signal = StopSignal::parse(signal).unwrap();
        assert_eq!(
            StopAction::default_timeout(signal),
            Some(Duration::from_secs(30)),
            "{signal}"
        );
    }
}

#[skuld::test]
fn a_timeout_beside_kill_is_refused_and_no_timeout_is_not() {
    let kill = |timeout| StopAction {
        send_signal: StopSignal::Kill,
        timeout,
    };
    assert_eq!(kill(None).defect(), None);
    let defect = kill(Some(Duration::from_secs(5))).defect().unwrap();
    assert!(defect.contains("cannot go with `send-signal: kill`"), "{defect}");
}

#[skuld::test]
fn a_timeout_a_manifest_could_not_write_is_a_defect_wherever_it_came_from() {
    let with = |timeout| StopAction {
        send_signal: StopSignal::Terminate,
        timeout: Some(timeout),
    };
    for timeout in [Duration::ZERO, Duration::from_millis(1500), Duration::from_secs(86_401)] {
        assert!(with(timeout).defect().is_some(), "{timeout:?}");
    }
    assert_eq!(with(Duration::from_secs(1)).defect(), None);
    assert_eq!(with(Duration::from_secs(86_400)).defect(), None);
    assert_eq!(StopAction::default().defect(), None);
}

// Windows =============================================================================================================

#[skuld::test]
fn windows_refuses_what_it_decides_itself() {
    let written = |send_signal, timeout| Written { send_signal, timeout };

    // A task: Task Scheduler decides both; the signal is reported first.
    let (field, message) = scm_defect(ScmDaemon::Task, written(true, true)).unwrap();
    assert_eq!(field, StopField::SendSignal);
    assert!(
        message.contains("`stop-action.send-signal`") && message.contains("backend-specific"),
        "{message}"
    );
    assert_eq!(
        scm_defect(ScmDaemon::Task, written(false, true)).unwrap().0,
        StopField::Timeout
    );
    assert_eq!(scm_defect(ScmDaemon::Task, written(false, false)), None);

    // A native service handles SCM's stop itself: a signal is refused, a timeout is not.
    assert_eq!(
        scm_defect(ScmDaemon::NativeService, written(true, false)).unwrap().0,
        StopField::SendSignal
    );
    assert_eq!(scm_defect(ScmDaemon::NativeService, written(false, true)), None);

    // A plain service takes both.
    assert_eq!(scm_defect(ScmDaemon::PlainService, written(true, true)), None);
}

#[skuld::test]
fn a_resolved_action_shows_what_differs_from_the_default() {
    assert_eq!(
        Written::of(&StopAction::default()),
        Written {
            send_signal: false,
            timeout: false
        }
    );
    let quit = StopAction {
        send_signal: StopSignal::Quit,
        timeout: Some(Duration::from_secs(30)),
    };
    assert_eq!(
        Written::of(&quit),
        Written {
            send_signal: true,
            timeout: false
        }
    );
    let patient = StopAction {
        send_signal: StopSignal::Terminate,
        timeout: None,
    };
    assert_eq!(
        Written::of(&patient),
        Written {
            send_signal: false,
            timeout: true
        }
    );
    // `kill`'s own default is no timeout.
    let kill = StopAction {
        send_signal: StopSignal::Kill,
        timeout: None,
    };
    assert_eq!(
        Written::of(&kill),
        Written {
            send_signal: true,
            timeout: false
        }
    );
}

// Reading the manifest ================================================================================================

#[skuld::test]
fn the_scalar_form_is_the_signal_alone() {
    assert_eq!(
        raw("stop-action: terminate\n").unwrap(),
        Some(RawStopAction {
            send_signal: Some("terminate".to_string()),
            timeout: None,
            ..RawStopAction::default()
        })
    );
}

#[skuld::test]
fn the_canonical_form_reads_both_leaves_as_text() {
    assert_eq!(
        raw("stop-action:\n  send-signal: interrupt\n  timeout: 2m\n").unwrap(),
        Some(RawStopAction {
            send_signal: Some("interrupt".to_string()),
            timeout: Some(RawTimeout::Text("2m".to_string())),
            ..RawStopAction::default()
        })
    );
}

#[skuld::test]
fn either_leaf_alone_and_an_empty_mapping_are_legal() {
    let action = raw("stop-action: {timeout: 5s}\n").unwrap().unwrap();
    assert_eq!(action.send_signal, None);
    assert_eq!(action.timeout, Some(RawTimeout::Text("5s".to_string())));
    assert_eq!(raw("stop-action: {}\n").unwrap(), Some(RawStopAction::default()));
    assert_eq!(raw("").unwrap(), None);
}

#[skuld::test]
fn a_null_timeout_is_a_value_in_every_spelling() {
    for spelling in ["null", "~", "Null", ""] {
        let action = raw(&format!("stop-action:\n  timeout: {spelling}\n")).unwrap().unwrap();
        assert_eq!(action.timeout, Some(RawTimeout::Null), "`{spelling}`");
    }
}

#[skuld::test]
fn a_quoted_null_is_text_and_so_not_a_duration() {
    let action = raw("stop-action:\n  timeout: \"null\"\n").unwrap().unwrap();
    assert_eq!(action.timeout, Some(RawTimeout::Text("null".to_string())));
}

#[skuld::test]
fn every_other_null_is_refused() {
    for yaml in [
        "stop-action: null\n",
        "stop-action: ~\n",
        "stop-action:\n",
        "stop-action:\n  send-signal: null\n",
        "stop-action:\n  send-signal:\n  timeout: 5s\n",
    ] {
        let message = raw(yaml).expect_err(yaml).to_string();
        assert!(message.contains("an explicit `null` is not"), "{yaml}: {message}");
    }
    assert!(
        raw("stop-action: null\n")
            .unwrap_err()
            .to_string()
            .contains("omit the `stop-action:` key")
    );
}

#[skuld::test]
fn an_unknown_or_repeated_leaf_is_an_error() {
    let message = raw("stop-action:\n  signal: quit\n").unwrap_err().to_string();
    assert!(
        message.contains("unknown field `signal`") && message.contains("send-signal"),
        "{message}"
    );
    assert!(
        raw("stop-action:\n  timeout: 1s\n  timeout: 2s\n")
            .unwrap_err()
            .to_string()
            .contains("duplicate field `timeout`")
    );
    assert!(
        raw("stop-action:\n  send-signal: quit\n  send-signal: quit\n")
            .unwrap_err()
            .to_string()
            .contains("duplicate field `send-signal`")
    );
    assert!(raw("stop-action: [terminate]\n").is_err());
}

#[skuld::test]
fn an_override_lays_its_leaves_over_the_base() {
    let base = Some(RawStopAction {
        send_signal: Some("quit".to_string()),
        timeout: Some(RawTimeout::Null),
        ..RawStopAction::default()
    });
    let timeout_only = RawStopAction {
        send_signal: None,
        timeout: Some(RawTimeout::Text("5s".to_string())),
        ..RawStopAction::default()
    };
    assert_eq!(
        timeout_only.over(&base),
        RawStopAction {
            send_signal: Some("quit".to_string()),
            timeout: Some(RawTimeout::Text("5s".to_string())),
            ..RawStopAction::default()
        }
    );
    let signal_only = RawStopAction {
        send_signal: Some("interrupt".to_string()),
        timeout: None,
        ..RawStopAction::default()
    };
    assert_eq!(
        signal_only.over(&base),
        RawStopAction {
            send_signal: Some("interrupt".to_string()),
            timeout: Some(RawTimeout::Null),
            timeout_from_base: true,
        }
    );
    assert_eq!(signal_only.over(&None), signal_only);
}
