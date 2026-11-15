//! Unit coverage for the pure launchd state classification in
//! `state.rs` — the whole reason this module is not `#[cfg]`-gated: these
//! tests run on every host this crate builds on, not only macOS.

use super::*;
use crate::cli::ExitCode;
use crate::manager::{Exit, State};
use crate::spec::Id;

/// A `launchctl print` body shaped like the real thing: tab-indented
/// `key = value` lines, with header fields (`active count`, `path`, `type`)
/// ahead of `state`, and `pid` further down still — [`classify`]'s two
/// fields of interest are not adjacent in the real output.
fn print_body(state: &str, pid: Option<u32>) -> String {
    let pid_line = match pid {
        Some(pid) => format!("\tpid = {pid}\n"),
        None => String::new(),
    };
    format!(
        "system/com.example.daemon = {{\n\
         \tactive count = 1\n\
         \tpath = /Library/LaunchDaemons/com.example.daemon.plist\n\
         \ttype = LaunchDaemon\n\
         \tstate = {state}\n\
         \n\
         \tprogram = /usr/local/bin/example-daemon\n\
         {pid_line}\
         }}\n"
    )
}

// classify ============================================================================================================

#[skuld::test]
fn running_is_running() {
    assert_eq!(classify(&print_body("running", Some(4242))).0, State::Running);
}

#[skuld::test]
fn xpcproxy_with_a_pid_is_running() {
    // The exact shape all 10 observed CI failures had.
    let body = print_body("xpcproxy", Some(39459));
    assert!(body.contains("active count = 1"));
    assert!(body.contains("state = xpcproxy"));
    assert!(body.contains("pid = 39459"));
    assert_eq!(classify(&body).0, State::Running);
}

#[skuld::test]
fn trampoline_with_a_pid_is_running() {
    assert_eq!(classify(&print_body("trampoline", Some(9001))).0, State::Running);
}

#[skuld::test]
fn xpcproxy_without_a_pid_is_unknown() {
    // Never observed in the 580-iteration sample, so `Running` would claim
    // more than was measured.
    assert_eq!(classify(&print_body("xpcproxy", None)).0, State::Unknown);
}

#[skuld::test]
fn not_running_is_stopped() {
    assert_eq!(classify(&print_body("not running", None)).0, State::Stopped);
}

#[skuld::test]
fn exited_is_stopped() {
    assert_eq!(classify(&print_body("exited", None)).0, State::Stopped);
}

#[skuld::test]
fn spawn_scheduled_is_unknown() {
    assert_eq!(classify(&print_body("spawn scheduled", None)).0, State::Unknown);
}

#[skuld::test]
fn an_unrecognised_state_is_unknown_not_stopped() {
    // A future macOS growing a new `state = ` value must only make goetia
    // vaguer, never wrong.
    assert_eq!(classify(&print_body("someday-new-apple-state", None)).0, State::Unknown);
}

#[skuld::test]
fn a_body_with_no_state_line_is_unknown() {
    let body = "system/com.example.daemon = {\n\tactive count = 1\n}\n";
    assert_eq!(classify(body).0, State::Unknown);
}

#[skuld::test]
fn the_pid_is_returned_alongside_every_state() {
    assert_eq!(classify(&print_body("not running", Some(4242))).1, Some(4242));
    assert_eq!(
        classify(&print_body("someday-new-apple-state", Some(4242))).1,
        Some(4242)
    );
}

// Evidence ============================================================================================================

#[skuld::test]
fn queued_or_running_accepts_a_queued_start_and_a_running_job() {
    let shown = |body: &str| Evidence::QueuedOrRunning.shown_by(body);
    assert!(shown(&print_body("spawn scheduled", None)));
    assert!(shown(&print_body("running", Some(4242))));
    assert!(shown(&print_body("xpcproxy", Some(39459))));
}

#[skuld::test]
fn no_evidence_accepts_any_other_reading() {
    for evidence in [Evidence::Queued, Evidence::QueuedOrRunning] {
        let shown = |body: &str| evidence.shown_by(body);
        assert!(!shown(&print_body("not running", None)), "{evidence:?}");
        assert!(!shown(&print_body("exited", None)), "{evidence:?}");
        assert!(!shown(&print_body("xpcproxy", None)), "{evidence:?}");
        assert!(!shown(&print_body("someday-new-apple-state", None)), "{evidence:?}");
        assert!(
            !shown("system/com.example.daemon = {\n\tactive count = 1\n}\n"),
            "{evidence:?}"
        );
        assert!(!shown(""), "{evidence:?}");
    }
}

#[skuld::test]
fn queued_accepts_only_a_queued_start() {
    assert!(Evidence::Queued.shown_by(&print_body("spawn scheduled", None)));
    assert!(!Evidence::Queued.shown_by(&print_body("running", Some(4242))));
    assert!(!Evidence::Queued.shown_by(&print_body("xpcproxy", Some(39459))));
}

// find_field ==========================================================================================================

#[skuld::test]
fn find_field_extracts_a_launchctl_print_style_line() {
    let text = "system/foo = {\n\tstate = running\n\tpid = 4242\n}\n";
    assert_eq!(find_field(text, "state"), Some("running"));
    assert_eq!(find_field(text, "pid"), Some("4242"));
}

#[skuld::test]
fn find_field_returns_none_for_a_missing_key() {
    let text = "system/foo = {\n\tstate = running\n}\n";
    assert_eq!(find_field(text, "pid"), None);
}

// kickstart_pid =======================================================================================================

/// The measured shape: `kickstart -p` writes the bare decimal pid and a
/// newline, and nothing else.
#[skuld::test]
fn kickstart_pid_reads_the_bare_decimal_form() {
    assert_eq!(kickstart_pid("4766\n"), Some(4766));
}

#[skuld::test]
fn kickstart_pid_rejects_empty_and_non_numeric_output() {
    assert_eq!(kickstart_pid(""), None);
    assert_eq!(kickstart_pid("\n"), None);
    assert_eq!(kickstart_pid("not a pid\n"), None);
    assert_eq!(kickstart_pid("4766 spawned\n"), None);
}

/// The rule the whole guard exists for. A prefix of a pid parses as a
/// *different* pid, and quite possibly a live one: `"4766\n"` cut mid-write
/// to `"47"` reads as `47`, and nothing about that number looks wrong. So the
/// whole `<digits>\n` line has to be present, or there is no confirmation —
/// otherwise the clock picks which process `start` claims it launched.
#[skuld::test]
fn kickstart_pid_rejects_a_truncated_line() {
    assert_eq!(kickstart_pid("47"), None);
    assert_eq!(kickstart_pid("4766"), None);
}

// tearing_down ========================================================================================================

/// The measured shape of a job whose `bootout` has returned and whose process has not exited.
#[skuld::test]
fn a_sigtermed_job_is_being_torn_down() {
    assert!(tearing_down(&print_body("SIGTERMed", Some(4242))));
    assert!(tearing_down(&print_body("SIGTERMed", None)));
}

/// Any other state, one this code does not recognise included, is a loaded job: a `start` does not
/// wait on something it cannot identify.
#[skuld::test]
fn any_other_state_is_not_a_job_being_torn_down() {
    for state in [
        "running",
        "not running",
        "exited",
        "spawn scheduled",
        "xpcproxy",
        "someday-new-apple-state",
    ] {
        assert!(!tearing_down(&print_body(state, Some(4242))), "{state}");
    }
    assert!(!tearing_down("no state line at all\n"));
}

// A task's `print` ====================================================================================================

/// A `print` body of a task's job: the header, the `state` and, where the job holds a process, its
/// `pid`, then `runs`, the result line (`result` is the whole line without its tab and newline) and
/// `started suspended`. The result line and the pid sit well apart, as in the real output.
fn task_print(state: &str, pid: Option<u32>, result: &str, suspended: Option<bool>) -> String {
    let pid_line = pid.map(|pid| format!("\tpid = {pid}\n")).unwrap_or_default();
    let suspended_line = suspended
        .map(|held| format!("\tstarted suspended = {}\n", u8::from(held)))
        .unwrap_or_default();
    format!(
        "system/com.example.task = {{\n\
         \tactive count = 1\n\
         \tpath = /Library/LaunchDaemons/com.example.task.plist\n\
         \ttype = LaunchDaemon\n\
         \tstate = {state}\n\
         \n\
         \tprogram = /usr/local/bin/example-task\n\
         \truns = 3\n\
         {pid_line}\
         \t{result}\n\
         {suspended_line}\
         }}\n"
    )
}

// classify_task -------------------------------------------------------------------------------------------------------

#[skuld::test]
fn classify_task_reads_spawn_scheduled_as_running_and_exited_as_stopped() {
    let queued = task_print("spawn scheduled", None, "last exit code = 0", None);
    // A start launchd has accepted and not yet forked: no pid, and a task is `Running` for it.
    assert_eq!(classify_task(&queued), (State::Running, None));
    // A service reads the same body as `Unknown`; it is the task's reading that differs.
    assert_eq!(classify(&queued), (State::Unknown, None));

    for state in ["exited", "not running"] {
        let body = task_print(state, None, "last exit code = 0", None);
        assert_eq!(classify_task(&body), (State::Idle, None), "{state}");
        assert_eq!(classify(&body), (State::Stopped, None), "{state}");
    }
}

#[skuld::test]
fn classify_task_keeps_every_other_reading_of_classify() {
    let running = task_print("running", Some(4242), "last exit code = 0", None);
    assert_eq!(classify_task(&running), (State::Running, Some(4242)));
    let starting = task_print("xpcproxy", Some(39459), "last exit code = 0", None);
    assert_eq!(classify_task(&starting), (State::Running, Some(39459)));
    for body in [
        task_print("xpcproxy", None, "last exit code = 0", None),
        task_print("someday-new-apple-state", None, "last exit code = 0", None),
        "system/com.example.task = {\n\tactive count = 1\n}\n".to_string(),
    ] {
        assert_eq!(classify_task(&body), classify(&body), "{body}");
        assert_eq!(classify_task(&body).0, State::Unknown, "{body}");
    }
}

// last_result ---------------------------------------------------------------------------------------------------------

/// The result line in each form P4 measured, read from a body as `print` gives it.
#[skuld::test]
fn last_result_reads_each_form_from_a_real_print() {
    let cases = [
        ("last exit code = 0", Some(Exit::Code(0))),
        ("last exit code = 3", Some(Exit::Code(3))),
        ("last terminating signal = Terminated: 15", Some(Exit::Signal(15))),
        ("last terminating signal = Killed: 9", Some(Exit::Signal(9))),
        ("last exit code = (never exited)", None),
    ];
    for (line, expected) in cases {
        let body = task_print("not running", None, line, None);
        assert_eq!(last_result(&body), expected, "{line}");
    }
}

/// While a run is in flight (a `pid` line) the result line is the run before's, and that is what is read.
#[skuld::test]
fn last_result_of_a_running_job_is_the_previous_runs() {
    let body = task_print("running", Some(4242), "last terminating signal = Killed: 9", None);
    assert_eq!(last_result(&body), Some(Exit::Signal(9)));
    let first = task_print("running", Some(4242), "last exit code = (never exited)", None);
    assert_eq!(last_result(&first), None);
}

#[skuld::test]
fn a_body_with_no_result_line_has_no_result() {
    assert_eq!(last_result("system/com.example.task = {\n\tstate = running\n}\n"), None);
    assert_eq!(last_result(""), None);
}

// has_process ---------------------------------------------------------------------------------------------------------

#[skuld::test]
fn has_process_reads_the_pid_for_every_state() {
    for state in [
        "running",
        "xpcproxy",
        "trampoline",
        "SIGTERMed",
        "someday-new-apple-state",
    ] {
        let body = task_print(state, Some(4242), "last exit code = 0", None);
        assert_eq!(has_process(&body), Some(4242), "{state}");
    }
    for state in ["spawn scheduled", "not running", "exited"] {
        let body = task_print(state, None, "last exit code = 0", None);
        assert_eq!(has_process(&body), None, "{state}");
    }
    assert_eq!(has_process(""), None);
}

// spawn_scheduled -----------------------------------------------------------------------------------------------------

#[skuld::test]
fn spawn_scheduled_is_the_state_line_alone() {
    assert!(spawn_scheduled(&task_print(
        "spawn scheduled",
        None,
        "last exit code = 0",
        None
    )));
    for state in [
        "running",
        "not running",
        "exited",
        "xpcproxy",
        "someday-new-apple-state",
    ] {
        assert!(
            !spawn_scheduled(&task_print(state, None, "last exit code = 0", None)),
            "{state}"
        );
    }
    assert!(!spawn_scheduled(""));
}

// exit_of -------------------------------------------------------------------------------------------------------------

/// A kqueue `NOTE_EXITSTATUS` event's data is a `wait(2)` status: the exit code is its second byte, and a
/// signal the low seven bits, with bit 7 for a core dump.
#[skuld::test]
fn exit_of_decodes_exited_and_signalled() {
    assert_eq!(exit_of(768), Exit::Code(3));
    assert_eq!(exit_of(0), Exit::Code(0));
    assert_eq!(exit_of(255 << 8), Exit::Code(255));
    assert_eq!(exit_of(15), Exit::Signal(15));
    assert_eq!(
        exit_of(9 | 0x80),
        Exit::Signal(9),
        "a core dump is the signal that caused it"
    );
}

// launch_of -----------------------------------------------------------------------------------------------------------

const TA: u64 = 1_000;
const PID: u32 = 4242;

fn id() -> Id {
    Id::try_from("frpc").expect("an id")
}

/// `launch_of` of a process that started at `started` and then stood as `then`.
fn launch(then: Then, started: u64) -> Launch {
    launch_of(Some((started, then)), TA, &id(), PID)
}

/// The error a launch that ends the run carries.
fn ends(launch: Launch) -> Error {
    match launch {
        Launch::Ends(error) => error,
        other => panic!("the run was to end, got {other:?}"),
    }
}

/// Whether `error` is the stranded hold, which says a hold is pending.
fn is_stranded(error: &Error) -> bool {
    matches!(error, Error::Step { what, .. } if what.starts_with("`launchctl debug` held the next launch of `frpc`"))
}

/// A process that is alive after its exec is told by two kernel facts: whether it is stopped, and whether
/// it started after `ta`, the time read after our `debug` returned. Stopped is a held launch, and one that
/// started after `ta` began after our `debug` too.
#[skuld::test]
fn launch_of_tells_our_held_launch_from_an_older_one_taking_a_hold_or_not() {
    let alive = |stopped| Then::Process { stopped };
    assert!(matches!(launch(alive(true), TA + 1), Launch::Held));
    assert!(
        matches!(launch(alive(true), TA), Launch::TookHold),
        "a start at ta is not after it"
    );
    assert!(matches!(launch(alive(true), TA - 1), Launch::TookHold));
    assert!(matches!(launch(alive(false), TA), Launch::InFlight));
    assert!(matches!(launch(alive(false), 1), Launch::InFlight));
}

/// Our own launch that is running and not held was started by our `kickstart` with no hold on it: not
/// followed, and no hold is pending, so nothing is stranded.
#[skuld::test]
fn our_launch_that_is_running_is_unfollowed_and_strands_nothing() {
    let e = ends(launch(Then::Process { stopped: false }, TA + 1));

    assert!(!is_stranded(&e), "{e:?}");
    assert!(
        matches!(&e, Error::RunUnrecorded { id, reason }
            if id == "frpc" && reason.contains(&format!("launched process {PID} with no hold on it"))),
        "{e:?}"
    );
    assert_eq!(ExitCode::of(&e), ExitCode::Indeterminate);
}

/// A launch that started at or before `ta` is not this run's, so however it ended, `run` makes another
/// pass: it never ends the run.
#[skuld::test]
fn an_older_launch_that_is_gone_starts_another_pass() {
    for started in [1, TA - 1, TA] {
        for status in [None, Some(3 << 8)] {
            assert!(
                matches!(launch(Then::Gone(status), started), Launch::Ended),
                "gone with {status:?}, started at {started}"
            );
        }
    }
}

/// A launch that started after `ta` is `run`'s own: gone at the path read, exited before its first exec
/// or gone at the stopped read, it ended before goetia could follow it. It is one error whichever it was,
/// with the status of the process where the watch kept one, and not the stranded hold: a launch that ends
/// in `xpcproxy` consumes the hold it took (KB, "macOS - A held launch killed in xpcproxy consumes its hold,
/// and the next launch is not held").
#[skuld::test]
fn our_launch_that_is_gone_ended_before_goetia_could_follow_it() {
    for (status, said) in [
        (None, ""),
        (Some(3 << 8), " (the process's status: exit code 3)"),
        (Some(9), " (the process's status: signal 9)"),
    ] {
        let e = ends(launch(Then::Gone(status), TA + 1));

        assert!(!is_stranded(&e), "{e:?}");
        let expected = format!("its launch ended before goetia could follow it{said}: ");
        assert!(
            matches!(&e, Error::RunUnrecorded { id, reason } if id == "frpc" && reason.starts_with(&expected)),
            "{e:?}"
        );
        assert_eq!(ExitCode::of(&e), ExitCode::Indeterminate);
    }
}

/// No start time: the pid was gone when it was registered, or at the read of its start. Whose launch it was
/// is not known, so the run ends in doubt, in the stranded hold.
#[skuld::test]
fn a_launch_with_no_start_time_is_in_doubt_in_the_stranded_hold() {
    let e = ends(launch_of(None, TA, &id(), PID));

    assert!(is_stranded(&e), "{e:?}");
    assert!(
        matches!(&e, Error::Step { source, .. }
            if matches!(&**source, Error::RequestInDoubt { request, reached: true, .. }
                if request == "launchctl kickstart -p system/frpc")),
        "{e:?}"
    );
    assert_eq!(ExitCode::of(&e), ExitCode::Indeterminate);
}

/// A pid `launchctl procinfo` does not name as the job's launch is another process's: the same end as a
/// pid that was gone, whenever it started and whether or not it is stopped.
#[skuld::test]
fn a_pid_that_is_another_process_is_in_doubt_in_the_stranded_hold() {
    for started in [1, TA, TA + 1] {
        let e = ends(launch(Then::Stranger, started));

        assert!(is_stranded(&e), "started at {started}: {e:?}");
        assert!(
            matches!(&e, Error::Step { source, .. } if matches!(**source, Error::RequestInDoubt { .. })),
            "{e:?}"
        );
        assert_eq!(ExitCode::of(&e), ExitCode::Indeterminate);
    }
}
