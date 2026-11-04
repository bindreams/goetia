use std::path::PathBuf;
use std::time::Duration;

use super::{Error, VersionReading};

#[skuld::test]
fn indeterminate_errors_are_exactly_the_in_doubt_ones() {
    let id = || "x".to_string();
    let in_doubt = [
        Error::Undetermined {
            id: id(),
            reason: String::new(),
            recovery: String::new(),
        },
        Error::WaitTimeout {
            id: id(),
            awaited: "running",
            waited: Duration::ZERO,
            recovery: String::new(),
        },
        Error::Unestablished {
            id: id(),
            detail: String::new(),
        },
        Error::RequestInDoubt {
            request: String::new(),
            reached: false,
            detail: String::new(),
        },
        Error::LoadedJobInDoubt {
            id: id(),
            restore_failed: None,
        },
        Error::LoadedStateInDoubt {
            id: id(),
            detail: String::new(),
            restore_failed: None,
        },
        Error::StaleJobBootedOut {
            id: id(),
            path: PathBuf::new(),
            found: String::new(),
        },
    ];
    for e in &in_doubt {
        assert!(e.is_indeterminate(), "{e:?}");
    }

    let settled = [
        Error::Os {
            what: String::new(),
            path: None,
            source: std::io::Error::other("x").into(),
        },
        Error::EnvFile {
            path: PathBuf::new(),
            line: 1,
            message: String::new(),
        },
        Error::Interpolate {
            path: String::new(),
            message: String::new(),
        },
        Error::Yaml(yaml_serde::from_str::<yaml_serde::Value>("a: [").unwrap_err()),
        Error::Invalid {
            daemon: id(),
            message: String::new(),
        },
        Error::Blob(String::new()),
        Error::NotInstalled { id: id() },
        Error::Foreign {
            id: id(),
            recovery: String::new(),
        },
        Error::NotRunning { id: id() },
        Error::NoManager {
            evidence: String::new(),
        },
        Error::ElevationRequired {
            subcommand: String::new(),
        },
        Error::UnsupportedPlatform {
            platform: String::new(),
        },
        Error::Command {
            command: String::new(),
            status: exit_status(1),
            stderr: String::new(),
        },
        Error::Spawn {
            command: String::new(),
            source: std::io::Error::other("x").into(),
        },
        Error::AccountLookup { detail: String::new() },
        Error::AlreadyExists { path: PathBuf::new() },
        Error::ConcurrentChange { id: id(), path: None },
        Error::QuarantineStranded {
            id: id(),
            quarantine: PathBuf::new(),
            occupied: PathBuf::new(),
        },
        Error::AmbiguousInstall {
            id: id(),
            paths: [PathBuf::new(), PathBuf::new()],
        },
        Error::NotStarted { id: id() },
        Error::LoadedJob { id: id() },
        Error::PasswordRequired { account: id() },
        Error::UnsupportedManager {
            client: VersionReading::Major(241),
            manager: VersionReading::Unreadable(String::new()),
        },
        Error::NotEnqueued {
            request: String::new(),
            diagnostic: None,
        },
        Error::IncompleteAnswer {
            command: String::new(),
            missing: Vec::new(),
        },
        Error::UnitNotLoaded { unit: id() },
        Error::Reservation {
            id: None,
            source: std::io::Error::other("x"),
        },
        Error::NotADirectory { path: PathBuf::new() },
    ];
    for e in &settled {
        assert!(!e.is_indeterminate(), "{e:?}");
    }
}

#[cfg(unix)]
fn exit_status(code: i32) -> std::process::ExitStatus {
    std::os::unix::process::ExitStatusExt::from_raw(code << 8)
}

#[cfg(windows)]
fn exit_status(code: i32) -> std::process::ExitStatus {
    std::os::windows::process::ExitStatusExt::from_raw(code as u32)
}

fn os(what: &str, path: &str, cause: &str) -> Error {
    Error::Os {
        what: what.to_string(),
        path: Some(PathBuf::from(path)),
        source: std::io::Error::other(cause.to_string()).into(),
    }
}

fn not_installed() -> Error {
    Error::NotInstalled { id: "x".to_string() }
}

fn in_doubt() -> Error {
    Error::WaitTimeout {
        id: "x".to_string(),
        awaited: "running",
        waited: Duration::ZERO,
        recovery: String::new(),
    }
}

#[skuld::test]
fn report_joins_the_chain_once() {
    assert_eq!(
        os("read", "/a/b", "boom").report().to_string(),
        "failed to read /a/b: boom"
    );
}

#[skuld::test]
fn report_joins_each_level_of_a_step() {
    let e = Error::Step {
        what: "regenerate `x`".to_string(),
        source: Box::new(os("read", "/a", "boom")),
    };
    assert_eq!(e.report().to_string(), "regenerate `x`: failed to read /a: boom");
}

#[skuld::test]
fn report_prints_both_halves() {
    let e = Error::Both {
        first: Box::new(os("remove", "/a", "boom")),
        then: Box::new(not_installed()),
    };
    assert_eq!(
        e.report().to_string(),
        "failed to remove /a: boom; then: daemon `x` is not installed"
    );
}

#[skuld::test]
fn report_prints_a_both_under_a_step_once() {
    let e = Error::Step {
        what: "uninstall".to_string(),
        source: Box::new(Error::Both {
            first: Box::new(os("remove", "/a", "boom")),
            then: Box::new(os("reload", "/b", "bang")),
        }),
    };
    let text = e.report().to_string();
    assert_eq!(
        text,
        "uninstall: failed to remove /a: boom; then: failed to reload /b: bang"
    );
    assert_eq!(text.matches("boom").count(), 1, "{text}");
}

#[skuld::test]
fn report_prints_an_in_doubt_restore_failure_once() {
    let e = Error::LoadedJobInDoubt {
        id: "x".to_string(),
        restore_failed: Some(Box::new(os("write", "/p", "disk full"))),
    };
    let text = e.report().to_string();
    assert_eq!(text.matches("disk full").count(), 1, "{text}");
    assert!(text.ends_with("failed to write /p: disk full"), "{text}");
    assert_eq!(e.to_string().matches("disk full").count(), 0);
}

#[skuld::test]
fn cause_reads_through_step() {
    let e = Error::Step {
        what: "a".to_string(),
        source: Box::new(Error::Step {
            what: "b".to_string(),
            source: Box::new(not_installed()),
        }),
    };
    assert!(matches!(e.cause(), Error::NotInstalled { .. }), "{:?}", e.cause());
    assert!(matches!(not_installed().cause(), Error::NotInstalled { .. }));
}

#[skuld::test]
fn both_is_indeterminate_only_when_both_halves_are() {
    let both = |first: Error, then: Error| Error::Both {
        first: Box::new(first),
        then: Box::new(then),
    };
    assert!(both(in_doubt(), in_doubt()).is_indeterminate());
    assert!(!both(in_doubt(), not_installed()).is_indeterminate());
    assert!(!both(not_installed(), in_doubt()).is_indeterminate());
    assert!(!both(not_installed(), not_installed()).is_indeterminate());
}

#[skuld::test]
fn a_step_over_an_indeterminate_error_is_indeterminate() {
    let step = |source: Error| Error::Step {
        what: "x".to_string(),
        source: Box::new(source),
    };
    assert!(step(in_doubt()).is_indeterminate());
    assert!(!step(not_installed()).is_indeterminate());
}
