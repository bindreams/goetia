use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::{BoxError, Error, Occupant, PutBackFound, VersionReading};

#[skuld::test]
fn indeterminate_errors_are_exactly_the_in_doubt_ones() {
    let id = || "x".to_string();
    let in_doubt = [
        Error::Undetermined {
            id: id(),
            reason: Box::new(os("read", "/p", "x")),
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
            refused: Box::new(not_installed()),
        },
        Error::RequestInDoubt {
            request: String::new(),
            reached: false,
            note: None,
            source: std::io::Error::other("x").into(),
        },
        Error::LoadedJobInDoubt {
            id: id(),
            restore_failed: None,
        },
        Error::LoadedStateInDoubt {
            id: id(),
            read_failed: Box::new(not_installed()),
            restore_failed: None,
        },
        Error::StaleJobBootedOut {
            id: id(),
            path: PathBuf::new(),
            found: Occupant::File,
        },
        Error::StartUnconfirmed { id: id() },
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
            source: None,
        },
        Error::blob(String::new()),
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
        Error::AccountLookup {
            detail: String::new(),
            source: None,
        },
        Error::StartAsManaged {
            blob: Arc::new(Error::blob(String::new())),
            source: Box::new(not_installed()),
        },
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
        Error::NotStarted {
            id: id(),
            keep_alive: false,
        },
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
        Error::InvalidPassword { variable: id() },
        Error::PutBackBlocked {
            path: PathBuf::new(),
            found: PutBackFound::Gone,
        },
    ];
    for e in &settled {
        assert!(!e.is_indeterminate(), "{e:?}");
    }
}

#[skuld::test]
fn a_start_judged_as_managed_is_indeterminate_when_the_failed_start_is() {
    let over = |source: Error| Error::StartAsManaged {
        blob: Arc::new(Error::blob(String::new())),
        source: Box::new(source),
    };
    assert!(over(in_doubt()).is_indeterminate());
    assert!(!over(not_installed()).is_indeterminate());
}

#[cfg(unix)]
pub(crate) fn exit_status(code: i32) -> std::process::ExitStatus {
    std::os::unix::process::ExitStatusExt::from_raw(code << 8)
}

#[cfg(windows)]
pub(crate) fn exit_status(code: i32) -> std::process::ExitStatus {
    std::os::windows::process::ExitStatusExt::from_raw(code as u32)
}

pub(crate) fn os(what: &str, path: &str, cause: &str) -> Error {
    Error::Os {
        what: what.to_string(),
        path: Some(PathBuf::from(path)),
        source: std::io::Error::other(cause.to_string()).into(),
    }
}

pub(crate) fn not_installed() -> Error {
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

fn step(what: &str, source: Error) -> Error {
    Error::Step {
        what: what.to_string(),
        source: Box::new(source),
    }
}

fn start_as_managed(source: Error) -> Error {
    Error::StartAsManaged {
        blob: Arc::new(Error::blob("corrupt")),
        source: Box::new(source),
    }
}

/// A start judged as managed is its failed start, as `is_indeterminate` already reads it.
#[skuld::test]
fn cause_reads_through_a_start_judged_as_managed() {
    let e = step("start", start_as_managed(step("wait", in_doubt())));
    assert!(matches!(e.cause(), Error::WaitTimeout { .. }), "{:?}", e.cause());
}

#[skuld::test]
fn map_cause_replaces_the_cause_and_keeps_every_wrapper() {
    let e = step("start", start_as_managed(step("wait", in_doubt())));

    let mapped = e.map_cause(|leaf| {
        assert!(matches!(leaf, Error::WaitTimeout { .. }), "{leaf:?}");
        not_installed()
    });

    assert!(matches!(mapped.cause(), Error::NotInstalled { .. }), "{mapped:?}");
    assert_eq!(
        mapped.report().to_string(),
        "start: its spec blob could not be read, so the start was judged as a `type: managed` service's (blob: \
         corrupt): wait: daemon `x` is not installed"
    );
}

/// `Both` has two causes, so it is a cause itself: neither walk goes into it.
#[skuld::test]
fn neither_walk_goes_into_a_both() {
    let both = || Error::Both {
        first: Box::new(in_doubt()),
        then: Box::new(not_installed()),
    };
    assert!(matches!(both().cause(), Error::Both { .. }));
    let mapped = both().map_cause(|leaf| {
        assert!(matches!(leaf, Error::Both { .. }), "{leaf:?}");
        not_installed()
    });
    assert!(matches!(mapped, Error::NotInstalled { .. }), "{mapped:?}");
}

/// An error a snapshot kept is handed out as it was: the same text, source, remedy and class.
#[skuld::test]
fn a_kept_error_reads_as_the_error_it_keeps() {
    let kept = Arc::new(Error::blob_from("invalid blob JSON", std::io::Error::other("EOF")));
    let owned = Error::Kept(Arc::clone(&kept));

    assert!(std::error::Error::source(&owned).is_some(), "{owned:?}");
    assert_eq!(owned.report().to_string(), "blob: invalid blob JSON: EOF");
    assert_eq!(owned.report().to_string(), kept.report().to_string());
    assert_eq!(owned.remedy(), kept.remedy());
    assert_eq!(owned.is_indeterminate(), kept.is_indeterminate());

    let undetermined = Error::Kept(Arc::new(undetermined("re-run as root")));
    assert_eq!(undetermined.remedy(), Some("re-run as root"));
    assert!(undetermined.is_indeterminate());
}

/// The errors written into a variant's own `Display` are exactly those `embedded` names, so
/// `report` can collect their remedies.
#[skuld::test]
fn embedded_names_the_error_a_variant_writes_into_its_own_text() {
    let inner = || undetermined("re-run as root");
    let loaded = Error::LoadedStateInDoubt {
        id: id(),
        read_failed: Box::new(inner()),
        restore_failed: None,
    };
    assert!(matches!(loaded.embedded(), Some(Error::Undetermined { .. })));
    assert!(matches!(
        start_as_managed(not_installed()).embedded(),
        Some(Error::Blob { .. })
    ));
    assert!(matches!(
        Error::Kept(Arc::new(loaded)).embedded(),
        Some(Error::Undetermined { .. })
    ));
    assert!(not_installed().embedded().is_none());
    assert!(step("a", inner()).embedded().is_none());
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

// Wording of each variant's report ====================================================================================

pub(crate) fn id() -> String {
    "x".to_string()
}

#[skuld::test]
fn report_of_a_failed_command_ends_in_its_stderr_only_when_it_wrote_some() {
    let status = exit_status(1);
    let e = |stderr: &str| Error::Command {
        command: "systemctl start x".to_string(),
        status,
        stderr: stderr.to_string(),
    };
    assert_eq!(
        e("").report().to_string(),
        format!("`systemctl start x` failed ({status})")
    );
    assert_eq!(
        e("Access denied").report().to_string(),
        format!("`systemctl start x` failed ({status}): Access denied")
    );
}

#[skuld::test]
fn report_of_a_command_that_could_not_run_prints_its_cause_once() {
    let e = Error::Spawn {
        command: "systemctl start x".to_string(),
        source: std::io::Error::other("no such file").into(),
    };
    assert_eq!(
        e.report().to_string(),
        "failed to run `systemctl start x`: no such file"
    );
}

#[skuld::test]
fn report_of_a_concurrent_change_names_where_it_was_read() {
    assert_eq!(
        Error::ConcurrentChange { id: id(), path: None }.report().to_string(),
        "`x` changed after goetia read it, so goetia did nothing further: re-run the command"
    );
    assert_eq!(
        Error::ConcurrentChange {
            id: id(),
            path: Some(PathBuf::from("/p"))
        }
        .report()
        .to_string(),
        "`x` changed after goetia read it at /p, so goetia did nothing further: re-run the command"
    );
}

#[skuld::test]
fn report_of_a_stranded_quarantine() {
    let e = Error::QuarantineStranded {
        id: id(),
        quarantine: PathBuf::from("/q"),
        occupied: PathBuf::from("/u"),
    };
    assert_eq!(
        e.report().to_string(),
        "the original fragment of `x` is quarantined at /q, but a different file occupies /u, so it cannot go back: \
         resolve manually"
    );
}

#[skuld::test]
fn report_of_an_ambiguous_install() {
    let e = Error::AmbiguousInstall {
        id: id(),
        paths: [PathBuf::from("/a"), PathBuf::from("/b")],
    };
    assert_eq!(
        e.report().to_string(),
        "daemon `x` has a plist in both /a and /b; remove one by hand (they should never both exist) before retrying"
    );
}

#[skuld::test]
fn report_of_a_start_that_left_no_process() {
    assert_eq!(
        Error::NotStarted {
            id: id(),
            keep_alive: false
        }
        .report()
        .to_string(),
        "`x` did not start: the manager accepted the start, but no process is running"
    );
    assert_eq!(
        Error::NotStarted {
            id: id(),
            keep_alive: true
        }
        .report()
        .to_string(),
        "`x` did not start: the manager accepted the start, but no process is running, and `restart: always` means \
         it should have one"
    );
}

#[skuld::test]
fn report_of_a_start_whose_state_is_not_established() {
    assert_eq!(
        Error::StartUnconfirmed { id: id() }.report().to_string(),
        "`x` may not have started: the manager accepted the start, but its state is not established"
    );
}

#[skuld::test]
fn report_of_a_loaded_job() {
    assert_eq!(
        Error::LoadedJob { id: id() }.report().to_string(),
        "`x` is loaded in launchd, which cannot update a loaded job, so the changed plist was not installed and the \
         installed plist is untouched: run `goetia daemon stop x` (a job that has exited is still loaded), re-run \
         install, then start it"
    );
}

#[skuld::test]
fn report_of_a_missing_password() {
    assert_eq!(
        Error::PasswordRequired {
            account: "DOM\\bob".to_string()
        }
        .report()
        .to_string(),
        "service account `DOM\\bob` needs a password (it is not LocalSystem/LocalService/NetworkService, nor an \
         `NT SERVICE\\`/`NT AUTHORITY\\` account); set GOETIA_SERVICE_PASSWORD before installing"
    );
}

#[skuld::test]
fn report_of_a_request_that_enqueued_nothing() {
    let e = |diagnostic: Option<&str>| Error::NotEnqueued {
        request: "systemctl start x".to_string(),
        diagnostic: diagnostic.map(str::to_string),
    };
    assert_eq!(
        e(Some("no such unit")).report().to_string(),
        "`systemctl start x` exited 0 without enqueuing a job, so the manager did not act on it: no such unit"
    );
    assert_eq!(
        e(None).report().to_string(),
        "`systemctl start x` exited 0 without enqueuing a job, so the manager did not act on it and said nothing \
         about why"
    );
}

#[skuld::test]
fn report_of_an_incomplete_answer() {
    let e = Error::IncompleteAnswer {
        command: "systemctl show x".to_string(),
        missing: vec!["LoadState".to_string(), "ActiveState".to_string()],
    };
    assert_eq!(
        e.report().to_string(),
        "`systemctl show x` answered without LoadState, ActiveState, so goetia cannot tell its live state"
    );
}

#[skuld::test]
fn report_of_a_unit_the_manager_has_not_loaded() {
    assert_eq!(
        Error::UnitNotLoaded {
            unit: "x.service".to_string()
        }
        .report()
        .to_string(),
        "systemd has not loaded the unit file goetia finds installed for `x.service` (`LoadState=not-found`), so what \
         it reports is not that unit's state"
    );
}

#[skuld::test]
fn report_of_a_reservation_prints_its_cause_once() {
    let e = |id: Option<&str>| Error::Reservation {
        id: id.map(str::to_string),
        source: std::io::Error::other("boom"),
    };
    assert_eq!(
        e(Some("x")).report().to_string(),
        "`x`: no thread or temp file could be made for a request, so none was sent: boom"
    );
    assert_eq!(
        e(None).report().to_string(),
        "no thread or temp file could be made for a request, so none was sent: boom"
    );
}

#[skuld::test]
fn report_of_a_path_that_is_not_a_directory() {
    assert_eq!(
        Error::NotADirectory {
            path: PathBuf::from("/p")
        }
        .report()
        .to_string(),
        "/p already exists and is not a directory; goetia cannot use it as a daemon's cwd or as its log directory"
    );
}

#[skuld::test]
fn report_of_an_invalid_password_variable_has_no_source() {
    let e = Error::InvalidPassword {
        variable: "GOETIA_SERVICE_PASSWORD".to_string(),
    };
    assert_eq!(
        e.report().to_string(),
        "`GOETIA_SERVICE_PASSWORD` is set but its value is not valid Unicode"
    );
    assert!(std::error::Error::source(&e).is_none());
}

#[skuld::test]
fn report_of_a_blocked_put_back_says_gone_or_replaced() {
    let e = |found| Error::PutBackBlocked {
        path: PathBuf::from("/p"),
        found,
    };
    assert_eq!(e(PutBackFound::Gone).report().to_string(), "/p no longer exists");
    assert_eq!(
        e(PutBackFound::Replaced).report().to_string(),
        "/p no longer holds what install wrote"
    );
}

#[skuld::test]
fn report_of_a_loaded_state_in_doubt_prints_each_cause_once() {
    const OPEN: &str = "`x`'s plist was written, but whether launchd has its job loaded could not be read (failed to \
         read /r: read boom), so goetia cannot tell whether a loaded job holds the old definition or the new one. ";
    let backed_out = Error::LoadedStateInDoubt {
        id: id(),
        read_failed: Box::new(os("read", "/r", "read boom")),
        restore_failed: None,
    };
    assert_eq!(
        backed_out.report().to_string(),
        format!(
            "{OPEN}The plist is back as it was before this install, so the change is not installed: run `goetia \
             daemon stop x`, re-run install, then `goetia daemon start x`"
        )
    );

    let stuck = Error::LoadedStateInDoubt {
        id: id(),
        read_failed: Box::new(os("read", "/r", "read boom")),
        restore_failed: Some(Box::new(os("write", "/p", "disk full"))),
    };
    let text = stuck.report().to_string();
    assert_eq!(
        text,
        format!(
            "{OPEN}Check what is on disk, then run `goetia daemon stop x`, re-run install, then `goetia daemon start \
             x`. The plist could not be put back as it was before this install: failed to write /p: disk full"
        )
    );
    assert_eq!(text.matches("disk full").count(), 1, "{text}");
    assert_eq!(text.matches("read boom").count(), 1, "{text}");
}

#[skuld::test]
fn report_of_an_undetermined_id_names_the_failed_read_and_then_the_remedy() {
    let e = Error::Undetermined {
        id: id(),
        reason: Box::new(os("read", "/p", "denied")),
        recovery: "re-run as root".to_string(),
    };
    assert_eq!(
        e.report().to_string(),
        "cannot determine whether daemon `x` is installed: failed to read /p: denied. re-run as root"
    );
    assert_eq!(e.remedy(), Some("re-run as root"));
    assert!(
        !e.to_string().contains("re-run as root"),
        "Display drops the remedy: {e}"
    );
    assert!(matches!(
        std::error::Error::source(&e)
            .and_then(|s| s.downcast_ref::<Box<Error>>())
            .map(|b| &**b),
        Some(Error::Os { .. })
    ));
}

pub(crate) fn undetermined(recovery: &str) -> Error {
    Error::Undetermined {
        id: id(),
        reason: Box::new(os("read", "/p", "denied")),
        recovery: recovery.to_string(),
    }
}

#[skuld::test]
fn a_step_over_an_undetermined_id_prints_the_remedy_after_the_whole_chain() {
    let e = Error::Step {
        what: "check `x`".to_string(),
        source: Box::new(undetermined("re-run as root")),
    };
    assert_eq!(
        e.report().to_string(),
        "check `x`: cannot determine whether daemon `x` is installed: failed to read /p: denied. re-run as root"
    );
    assert_eq!(e.remedy(), None, "a step has no remedy of its own");
}

#[skuld::test]
fn a_remedy_is_printed_once_however_many_halves_carry_it() {
    let e = Error::Both {
        first: Box::new(undetermined("re-run as root")),
        then: Box::new(undetermined("re-run as root")),
    };
    let text = e.report().to_string();
    assert_eq!(text.matches("re-run as root").count(), 1, "{text}");
    assert!(text.ends_with(". re-run as root"), "{text}");
}

#[skuld::test]
fn report_of_a_failure_without_a_remedy_ends_in_its_cause() {
    assert_eq!(os("read", "/p", "denied").remedy(), None);
    assert_eq!(not_installed().remedy(), None);
}

#[skuld::test]
fn report_of_a_request_in_doubt_keeps_its_note_before_its_cause() {
    let e = |note: Option<&str>, reached| Error::RequestInDoubt {
        request: "systemctl start x".to_string(),
        reached,
        note: note.map(str::to_string),
        source: std::io::Error::other("lost").into(),
    };
    assert_eq!(
        e(None, true).report().to_string(),
        "`systemctl start x` reached the service manager, but goetia lost track of it before its outcome was \
         confirmed: lost"
    );
    assert_eq!(
        e(Some("wrote the unit"), false).report().to_string(),
        "`systemctl start x` may have started before it failed, so it may or may not have reached the service \
         manager: wrote the unit: lost"
    );
    assert!(std::error::Error::source(&e(None, true)).is_some());
}

#[skuld::test]
fn a_request_in_doubt_takes_a_context_ahead_of_its_note() {
    let e = Error::RequestInDoubt {
        request: "r".to_string(),
        reached: false,
        note: None,
        source: std::io::Error::other("lost").into(),
    };
    let once = e.in_doubt_context("first".to_string());
    let twice = once.in_doubt_context("second".to_string());
    assert!(twice.is_indeterminate());
    assert!(
        twice.report().to_string().ends_with("manager: second: first: lost"),
        "{}",
        twice.report()
    );
    assert!(matches!(
        not_installed().in_doubt_context("x".to_string()),
        Error::NotInstalled { .. }
    ));
}

#[skuld::test]
fn report_of_an_unestablished_restart_ends_in_the_refusal() {
    let e = Error::Unestablished {
        id: id(),
        refused: Box::new(os("start", "/p", "refused")),
    };
    assert_eq!(
        e.report().to_string(),
        "`x` was not restarted, and where it ended up is not established: the stop was issued without waiting for \
         it, and the start that followed was refused (`goetia daemon status x` shows the manager's current view): \
         failed to start /p: refused"
    );
    assert!(e.is_indeterminate());
}

#[skuld::test]
fn report_of_an_account_lookup_ends_in_its_cause_when_it_has_one() {
    let e = |source: Option<BoxError>| Error::AccountLookup {
        detail: "look up user `bob`".to_string(),
        source,
    };
    assert_eq!(
        e(Some(std::io::Error::other("EPERM").into())).report().to_string(),
        "account lookup failed: look up user `bob`: EPERM"
    );
    assert_eq!(
        e(None).report().to_string(),
        "account lookup failed: look up user `bob`"
    );
}

#[skuld::test]
fn report_of_a_start_judged_as_managed_names_the_unreadable_blob_and_then_the_failure() {
    let e = Error::StartAsManaged {
        blob: Arc::new(Error::blob("invalid blob JSON: EOF".to_string())),
        source: Box::new(os("start", "/p", "stopped")),
    };
    assert_eq!(
        e.report().to_string(),
        "its spec blob could not be read, so the start was judged as a `type: managed` service's (blob: invalid blob \
         JSON: EOF): failed to start /p: stopped"
    );
    assert!(!e.is_indeterminate());
}

#[skuld::test]
fn an_undetermined_step_chain_gives_up_its_read_and_recovery() {
    let step = Error::Step {
        what: "check".to_string(),
        source: Box::new(undetermined("re-run as root")),
    };
    let (reason, recovery) = step.into_undetermined().expect("the chain ends in Undetermined");
    assert_eq!(reason.report().to_string(), "check: failed to read /p: denied");
    assert_eq!(recovery, "re-run as root");

    let other = Error::Step {
        what: "check".to_string(),
        source: Box::new(not_installed()),
    };
    let back = other.into_undetermined().expect_err("not an Undetermined");
    assert_eq!(back.report().to_string(), "check: daemon `x` is not installed");
}

// The OS error an error carries ---------------------------------------------------------------------------------------

fn os_code(code: i32) -> Error {
    Error::Os {
        what: "read".to_string(),
        path: None,
        source: std::io::Error::from_raw_os_error(code).into(),
    }
}

#[skuld::test]
fn the_os_error_is_found_through_every_wrapper() {
    let step = |source: Error| Error::Step {
        what: "step".to_string(),
        source: Box::new(source),
    };
    let undetermined = Error::Undetermined {
        id: "x".to_string(),
        reason: Box::new(step(os_code(5))),
        recovery: String::new(),
    };
    assert_eq!(os_code(5).raw_os_error(), Some(5));
    assert_eq!(step(undetermined).raw_os_error(), Some(5));
    assert_eq!(Error::Kept(Arc::new(step(os_code(6)))).raw_os_error(), Some(6));
    assert_eq!(step(not_installed()).raw_os_error(), None);
    assert_eq!(os("read", "/p", "no code").raw_os_error(), None);
}

/// Neither half of a `Both` is its `source`; the first with an OS error wins.
#[skuld::test]
fn the_os_error_of_a_both_is_the_first_halfs_else_the_seconds() {
    let both = |first: Error, then: Error| Error::Both {
        first: Box::new(first),
        then: Box::new(then),
    };
    let nested = Error::Step {
        what: "outer".to_string(),
        source: Box::new(both(not_installed(), os_code(7))),
    };

    assert_eq!(both(os_code(5), os_code(7)).raw_os_error(), Some(5));
    assert_eq!(both(not_installed(), os_code(7)).raw_os_error(), Some(7));
    assert_eq!(nested.raw_os_error(), Some(7));
    assert_eq!(both(not_installed(), not_installed()).raw_os_error(), None);
}

/// An error a variant writes into its own text is not its `source`.
#[skuld::test]
fn the_os_error_of_an_embedded_error_is_found() {
    let embedded = Error::LoadedStateInDoubt {
        id: "x".to_string(),
        read_failed: Box::new(os_code(8)),
        restore_failed: None,
    };
    let blob_unreadable = Error::StartAsManaged {
        blob: Arc::new(os_code(9)),
        source: Box::new(not_installed()),
    };

    assert_eq!(embedded.raw_os_error(), Some(8));
    assert_eq!(blob_unreadable.raw_os_error(), Some(9));
}
