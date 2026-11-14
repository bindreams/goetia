//! The journal seam: what an entry is made of, how a task's last run is read out of the entries
//! newest first, and the real `journalctl` driven through `sh`.

use super::fake::Fake;
use super::*;

// Entries =============================================================================================================

fn at(micros: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_micros(micros)
}

/// "Starting `<unit>`...", written at `micros`.
fn starting(invocation: &str, micros: u64) -> Entry {
    Entry::of(&[
        ("MESSAGE_ID", STARTING),
        ("INVOCATION_ID", invocation),
        ("__REALTIME_TIMESTAMP", &micros.to_string()),
    ])
}

/// "Main process exited", with `EXIT_CODE` and `EXIT_STATUS` as the manager writes them.
fn main_exited(invocation: &str, code: &str, status: &str) -> Entry {
    Entry::of(&[
        ("MESSAGE_ID", MAIN_PROCESS_EXITED),
        ("INVOCATION_ID", invocation),
        ("EXIT_CODE", code),
        ("EXIT_STATUS", status),
    ])
}

fn deactivated(invocation: &str) -> Entry {
    Entry::of(&[("MESSAGE_ID", DEACTIVATED), ("INVOCATION_ID", invocation)])
}

/// An entry that is about the unit and says nothing of a run's start or end ("Failed to start").
fn other(invocation: &str) -> Entry {
    Entry::of(&[
        ("MESSAGE_ID", "be02cf6855d2428ba40df7e9d022f03d"),
        ("INVOCATION_ID", invocation),
    ])
}

/// "Failed with result `result`": the unit failed, which a run whose process never exited ends with.
fn failed_with(invocation: &str, result: &str) -> Entry {
    Entry::of(&[
        ("MESSAGE_ID", FAILED_WITH_RESULT),
        ("INVOCATION_ID", invocation),
        ("UNIT_RESULT", result),
    ])
}

/// "Control process exited" (an `ExecStartPre=` and the like): the same `MESSAGE_ID` and fields as a
/// main process's, told apart by `COMMAND`.
fn control_exited(invocation: &str, status: &str) -> Entry {
    Entry::of(&[
        ("MESSAGE_ID", MAIN_PROCESS_EXITED),
        ("INVOCATION_ID", invocation),
        ("EXIT_CODE", "exited"),
        ("EXIT_STATUS", status),
        ("COMMAND", "ExecStartPre"),
    ])
}

fn run(entries: Vec<Entry>) -> Result<LastRun> {
    last_run(&Fake::new(entries), "x.service")
}

#[skuld::test]
fn last_result_reads_the_newest_run() {
    let ended = |exit| LastRun {
        started: Some(at(1_000)),
        exit: Some(exit),
    };
    // Each walk is newest first: the run's terminal entry, then its Starting entry.
    for (what, entries, expected) in [
        (
            "exited/3",
            vec![main_exited("a", "exited", "3"), starting("a", 1_000)],
            ended(Exit::Code(3)),
        ),
        (
            "killed/15",
            vec![main_exited("a", "killed", "15"), starting("a", 1_000)],
            ended(Exit::Signal(15)),
        ),
        (
            "dumped/11",
            vec![main_exited("a", "dumped", "11"), starting("a", 1_000)],
            ended(Exit::Signal(11)),
        ),
        (
            "Deactivated successfully",
            vec![deactivated("a"), starting("a", 1_000)],
            ended(Exit::Code(0)),
        ),
        (
            "entries that are neither the start nor the end are walked past",
            vec![
                other("a"),
                main_exited("a", "exited", "3"),
                other("a"),
                starting("a", 1_000),
            ],
            ended(Exit::Code(3)),
        ),
    ] {
        assert_eq!(run(entries).expect(what), expected, "{what}");
    }
}

#[skuld::test]
fn an_older_runs_terminal_entry_is_not_taken_for_the_newest_runs() {
    // `b` is the newest run and has not ended: `a`'s exit, which is newer than `b`'s Starting entry,
    // is `a`'s.
    let last = run(vec![
        main_exited("a", "exited", "3"),
        starting("b", 2_000),
        starting("a", 1_000),
    ])
    .expect("run");

    assert_eq!(
        last,
        LastRun {
            started: Some(at(2_000)),
            exit: None
        }
    );
}

#[skuld::test]
fn a_newest_run_with_no_terminal_entry_has_started_and_no_result() {
    let last = run(vec![starting("a", 1_000), main_exited("older", "exited", "3")]).expect("run");

    assert_eq!(
        last,
        LastRun {
            started: Some(at(1_000)),
            exit: None
        }
    );
}

#[skuld::test]
fn no_entries_is_no_run() {
    let none = LastRun {
        started: None,
        exit: None,
    };
    assert_eq!(run(Vec::new()).expect("run"), none);
    // A terminal entry with no Starting entry to belong to is no run either.
    assert_eq!(run(vec![main_exited("a", "exited", "3")]).expect("run"), none);
}

/// The walk is stopped at the answer, so a journal of years is not read for a unit that ran a
/// minute ago.
#[skuld::test]
fn the_walk_stops_at_the_starting_entry() {
    let fake = Fake::new(vec![
        main_exited("a", "exited", "3"),
        starting("a", 1_000),
        starting("old", 500),
        main_exited("old", "exited", "1"),
    ]);

    last_run(&fake, "x.service").expect("last_run");

    assert_eq!(fake.read(), 2, "it read past the newest Starting entry");
    assert_eq!(fake.asked(), [["UNIT=x.service"]]);
}

/// A failure of the read after the answer was found is never reached: the entries were dropped.
#[skuld::test]
fn a_journal_that_cannot_be_read_is_the_error_and_never_no_run() {
    let e = last_run(&Fake::unreadable(), "x.service").expect_err("unreadable");
    assert!(e.report().to_string().contains("no journal"), "{e}");

    let e = last_run(&Fake::new(vec![other("a")]).then_fails(), "x.service").expect_err("fails before the answer");
    assert!(e.report().to_string().contains("it failed"), "{e}");

    let found = last_run(&Fake::new(vec![starting("a", 1_000)]).then_fails(), "x.service");
    assert!(found.is_ok(), "the walk ended before the failure: {found:?}");
}

#[skuld::test]
fn a_terminal_entry_that_does_not_say_how_the_run_ended_is_the_error() {
    for (code, status) in [("exited", "x"), ("exited", ""), ("unknown", "3"), ("", "3")] {
        let e = run(vec![main_exited("a", code, status), starting("a", 1_000)]).expect_err(&format!("{code}/{status}"));
        assert!(matches!(e, Error::IncompleteAnswer { .. }), "{code}/{status}: {e:?}");
    }
    // An older run's unreadable entry is not the newest run's.
    let last = run(vec![
        starting("b", 2_000),
        main_exited("a", "exited", "x"),
        starting("a", 1_000),
    ]);
    assert!(last.is_ok(), "{last:?}");
}

#[skuld::test]
fn only_the_terminal_entries_end_a_run() {
    assert!(ending_of(&starting("a", 1)).is_none());
    assert!(ending_of(&other("a")).is_none());
    assert!(ending_of(&Entry::of(&[("MESSAGE", "no id")])).is_none());
    assert!(matches!(
        ending_of(&deactivated("a")),
        Some(Ending::Exit(Ok(Exit::Code(0))))
    ));
    assert!(matches!(
        ending_of(&failed_with("a", "resources")),
        Some(Ending::NeverRan { result }) if result == "resources"
    ));
}

/// A "Control process exited" entry (`ExecStartPre=` and the others) is not the run's exit; the main
/// process's, with `COMMAND=ExecStart` or none (a manager that does not write it), is.
#[skuld::test]
fn a_control_processs_exit_is_not_the_runs_exit() {
    assert!(ending_of(&control_exited("a", "1")).is_none());
    let main = Entry::of(&[
        ("MESSAGE_ID", MAIN_PROCESS_EXITED),
        ("INVOCATION_ID", "a"),
        ("EXIT_CODE", "exited"),
        ("EXIT_STATUS", "3"),
        ("COMMAND", "ExecStart"),
    ]);
    assert!(matches!(ending_of(&main), Some(Ending::Exit(Ok(Exit::Code(3))))));
    assert!(matches!(
        ending_of(&main_exited("a", "exited", "3")),
        Some(Ending::Exit(Ok(Exit::Code(3))))
    ));
}

/// A run whose process never exited ends with "Failed with result" and no exit entry: it has a start
/// and no result. Where an exit entry exists it is the older of the two, and the run's.
#[skuld::test]
fn a_run_that_failed_with_no_exit_has_a_start_and_no_result() {
    let never = LastRun {
        started: Some(at(1_000)),
        exit: None,
    };
    for (what, entries, expected) in [
        (
            "no exit entry",
            vec![other("a"), failed_with("a", "resources"), starting("a", 1_000)],
            never,
        ),
        (
            "a control process's exit does not count",
            vec![
                other("a"),
                failed_with("a", "exit-code"),
                control_exited("a", "1"),
                starting("a", 1_000),
            ],
            never,
        ),
        (
            "the exit entry is older than Failed with result, and wins",
            vec![
                other("a"),
                failed_with("a", "exit-code"),
                main_exited("a", "exited", "217"),
                starting("a", 1_000),
            ],
            LastRun {
                started: Some(at(1_000)),
                exit: Some(Exit::Code(217)),
            },
        ),
    ] {
        assert_eq!(run(entries).expect(what), expected, "{what}");
    }
}

// Entry ===============================================================================================================

#[skuld::test]
fn an_entry_is_read_from_a_line_of_json() {
    let entry = Entry::parse(
        br#"{"MESSAGE_ID":"7d49","MESSAGE":[72,105,255],"EMPTY":null,"TWICE":["first","second"],"__REALTIME_TIMESTAMP":"1794626807000123","PRIORITY":"6"}"#,
    )
    .expect("a line of journalctl's JSON");

    assert_eq!(entry.get("MESSAGE_ID"), Some("7d49"));
    assert_eq!(
        entry.bytes("MESSAGE"),
        Some(&[72, 105, 255][..]),
        "bytes decode to the bytes"
    );
    assert_eq!(entry.get("MESSAGE"), None, "and are no text where they are not UTF-8");
    assert_eq!(entry.get("EMPTY"), None);
    assert_eq!(entry.get("TWICE"), Some("first"));
    assert_eq!(entry.realtime().expect("a time"), at(1_794_626_807_000_123));
}

#[skuld::test]
fn a_line_that_is_not_an_entry_is_the_error() {
    for line in ["", "not json", "[1,2]", "\"x\"", "{\"A\":"] {
        assert!(Entry::parse(line.as_bytes()).is_err(), "{line:?}");
    }
    let no_time = Entry::of(&[("MESSAGE_ID", STARTING)]);
    assert!(no_time.realtime().is_err());
    let bad_time = Entry::of(&[("__REALTIME_TIMESTAMP", "soon")]);
    assert!(bad_time.realtime().is_err());
}

// Journalctl ==========================================================================================================

/// The journal as a shell script answers it: `script` is `journalctl`, and sees the query's
/// arguments as its own.
fn sh(script: &str) -> Journalctl {
    Journalctl::of(["/bin/sh", "-c", script, "journalctl"].map(String::from).to_vec())
}

fn entries_of(journal: &Journalctl) -> Result<Vec<Entry>> {
    let mut entries = journal.newest_first(&[Match::new("UNIT", "x.service")])?;
    let mut all = Vec::new();
    while let Some(entry) = entries.next()? {
        all.push(entry);
    }
    Ok(all)
}

#[skuld::test]
fn a_query_asks_for_all_of_every_field_as_json_newest_first() {
    let journal = sh(r#"printf '{"ARGS":"%s"}\n' "$*""#);

    let all = entries_of(&journal).expect("entries");

    assert_eq!(all.len(), 1);
    assert_eq!(
        all[0].get("ARGS"),
        Some("-a -o json --no-pager --system -r UNIT=x.service _PID=1")
    );
}

#[skuld::test]
fn every_line_is_an_entry_and_the_end_is_the_end() {
    let journal = sh(r#"printf '{"N":"1"}\n{"N":"2"}\n{"N":"3"}\n'"#);

    let all = entries_of(&journal).expect("entries");

    assert_eq!(
        all.iter().map(|entry| entry.get("N").unwrap()).collect::<Vec<_>>(),
        ["1", "2", "3"]
    );
}

#[skuld::test]
fn a_journal_with_no_entries_answers_none() {
    assert!(entries_of(&sh("exit 0")).expect("entries").is_empty());
}

/// What `journalctl` wrote on stderr is the reason, and its exit status is the verdict: nothing it
/// printed before is a complete answer.
#[skuld::test]
fn a_journalctl_that_fails_is_the_error_with_its_words() {
    let journal = sh(r#"printf '{"N":"1"}\n'; echo "Failed to open journal" >&2; exit 1"#);

    let e = entries_of(&journal).expect_err("journalctl failed");

    let msg = e.report().to_string();
    assert!(msg.contains("Failed to open journal"), "{msg}");
    assert!(
        msg.contains("journalctl -a -o json --no-pager --system -r UNIT=x.service _PID=1"),
        "{msg}"
    );
}

/// A line the child did not finish is no entry: the killed `journalctl` of a follow, a pipe cut short.
#[skuld::test]
fn a_partial_trailing_line_is_discarded() {
    let journal = sh(r##"printf '{"N":"1"}\n{"N":'"##);

    let all = entries_of(&journal).expect("entries");

    assert_eq!(all.len(), 1);
}

#[skuld::test]
fn a_journalctl_that_cannot_be_spawned_is_the_error() {
    let journal = Journalctl::of(vec!["/nonexistent/goetia-journalctl".to_string()]);

    let e = journal
        .newest_first(&[Match::new("UNIT", "x.service")])
        .err()
        .expect("no such program");

    assert!(matches!(e, Error::Spawn { .. }), "{e:?}");
}

/// Dropping the entries ends the `journalctl` behind them, so a walk that found its answer leaves
/// nothing running: the child is gone when the drop returns.
#[skuld::test]
fn dropping_the_entries_ends_the_journalctl() {
    let journal = sh(r#"printf '{"PID":"%s"}\n' "$$"; exec sleep 100000"#);
    let mut entries = journal.newest_first(&[Match::new("UNIT", "x.service")]).expect("spawn");

    let first = entries.next().expect("an entry").expect("the first line");
    let pid = first.get("PID").expect("the child's pid").to_string();
    assert!(std::path::Path::new(&format!("/proc/{pid}")).exists(), "it is running");
    drop(entries);

    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the journalctl outlived its entries"
    );
}

// follow, read_after, sync ============================================================================================

/// The one entry a script that echoes its arguments writes.
fn args_of(entries: Vec<Entry>) -> String {
    assert_eq!(entries.len(), 1, "{entries:?}");
    entries[0].get("ARGS").expect("the echoed arguments").to_string()
}

/// The reservation of the one temp file a followed query draws.
fn one() -> Reservation {
    Reservation::make(0, 1).expect("a temp file")
}

const ECHO: &str = r#"printf '{"ARGS":"%s"}\n' "$*""#;

/// The first entry of `follower`, which ends it.
fn first_of(mut follower: Box<dyn Follower>) -> Vec<Entry> {
    vec![follower.next().expect("an entry").expect("the first line")]
}

#[skuld::test]
fn every_query_passes_dash_a() {
    let journal = sh(ECHO);
    let job = [Match::new("UNIT", "x.service"), Match::new("JOB_ID", "7")];
    let unit = [Match::new("UNIT", "x.service")];
    let cursor = Cursor::of("s=1;i=2");

    for (what, args) in [
        ("newest first", args_of(entries_of(&journal).expect("entries"))),
        (
            "follow a job",
            args_of(first_of(
                journal.follow(Scope::Manager, &job, None, &mut one()).expect("follow"),
            )),
        ),
        (
            "follow after a cursor",
            args_of(first_of(
                journal
                    .follow(Scope::Manager, &unit, Some(&cursor), &mut one())
                    .expect("follow"),
            )),
        ),
        (
            "read after a cursor",
            args_of(
                journal
                    .read_after(Scope::Manager, &unit, &cursor, &mut one())
                    .expect("read_after"),
            ),
        ),
    ] {
        assert!(args.starts_with("-a -o json --no-pager --system "), "{what}: {args}");
    }
}

#[skuld::test]
fn a_follow_starts_from_the_beginning_or_after_its_cursor_and_job_numbers_are_bound_to_the_boot() {
    let journal = sh(ECHO);
    let cursor = Cursor::of("s=1;i=2");
    let unit = [Match::new("UNIT", "x.service")];

    let from_the_start = args_of(first_of(
        journal.follow(Scope::Manager, &unit, None, &mut one()).expect("follow"),
    ));
    let after = args_of(first_of(
        journal
            .follow(Scope::Manager, &unit, Some(&cursor), &mut one())
            .expect("follow"),
    ));
    let job = args_of(first_of(
        journal
            .follow(
                Scope::Manager,
                &[unit[0].clone(), Match::new("JOB_ID", "7")],
                None,
                &mut one(),
            )
            .expect("follow"),
    ));

    assert_eq!(
        from_the_start,
        "-a -o json --no-pager --system -f -n all UNIT=x.service _PID=1"
    );
    assert_eq!(
        after,
        "-a -o json --no-pager --system -f -n all --after-cursor=s=1;i=2 UNIT=x.service _PID=1"
    );
    assert_eq!(
        job,
        "-a -o json --no-pager --system -f -n all -b UNIT=x.service JOB_ID=7 _PID=1"
    );
}

/// Which queries carry `--system` and the trusted `_PID=1` match: the manager's (newest first, and the
/// follows that find a run's Starting and exit entries) do, since only PID 1 writes what they read and
/// a unit's own lines can forge a manager entry's fields; a unit's own output (a follow and a read
/// after a cursor of the `Output` scope) does not, since a unit that runs as a regular uid writes it
/// to a user journal, and it is not PID 1's.
#[skuld::test]
fn only_the_managers_queries_read_the_system_journal_alone_and_from_pid_1() {
    let journal = sh(ECHO);
    let unit = [Match::new("UNIT", "x.service")];
    let cursor = Cursor::of("c");
    let has = |args: &str, wanted: &str| args.split(' ').any(|argument| argument == wanted);

    let newest = args_of(entries_of(&journal).expect("entries"));
    assert!(
        has(&newest, "--system") && has(&newest, "_PID=1"),
        "newest first: {newest}"
    );
    for (scope, manager) in [(Scope::Manager, true), (Scope::Output, false)] {
        let follow = args_of(first_of(
            journal.follow(scope, &unit, Some(&cursor), &mut one()).expect("follow"),
        ));
        assert_eq!(has(&follow, "--system"), manager, "follow {scope:?}: {follow}");
        assert_eq!(has(&follow, "_PID=1"), manager, "follow {scope:?}: {follow}");
        let read = args_of(
            journal
                .read_after(scope, &unit, &cursor, &mut one())
                .expect("read_after"),
        );
        assert_eq!(has(&read, "--system"), manager, "read_after {scope:?}: {read}");
        assert_eq!(has(&read, "_PID=1"), manager, "read_after {scope:?}: {read}");
    }
}

#[skuld::test]
fn a_read_after_a_cursor_asks_for_every_match() {
    let journal = sh(ECHO);
    let matches = [
        Match::new("_SYSTEMD_UNIT", "x.service"),
        Match::new("_TRANSPORT", "stdout"),
    ];

    let args = args_of(
        journal
            .read_after(Scope::Output, &matches, &Cursor::of("c"), &mut one())
            .expect("read_after"),
    );

    assert_eq!(
        args,
        "-a -o json --no-pager --after-cursor=c _SYSTEMD_UNIT=x.service _TRANSPORT=stdout"
    );
}

#[skuld::test]
fn a_read_after_discards_a_partial_trailing_line_and_fails_with_journalctls_words() {
    let journal = sh(r##"printf '{"N":"1"}\n{"N":'"##);
    let all = journal
        .read_after(Scope::Manager, &[], &Cursor::of("c"), &mut one())
        .expect("read_after");
    assert_eq!(all.len(), 1);

    let journal = sh(r#"echo "Failed to seek" >&2; exit 1"#);
    let e = journal
        .read_after(Scope::Manager, &[], &Cursor::of("c"), &mut one())
        .expect_err("it failed");
    assert!(e.report().to_string().contains("Failed to seek"), "{e}");
}

#[skuld::test]
fn entries_decode_a_message_given_as_a_byte_array() {
    let entry = Entry::parse(br#"{"MESSAGE":[104,105,7,27,255],"__CURSOR":"c"}"#).expect("an entry");

    assert_eq!(entry.bytes("MESSAGE"), Some(&[104, 105, 7, 27, 255][..]));
    assert_eq!(entry.cursor().expect("a cursor"), Cursor::of("c"));
}

/// 4088 bytes is where `journalctl -o json` without `-a` gives `null`.
#[skuld::test]
fn a_message_of_4088_bytes_arrives_whole() {
    let journal = sh(r#"printf '{"MESSAGE":"'; head -c 4088 /dev/zero | tr '\0' x; printf '"}\n'"#);

    let all = journal
        .read_after(Scope::Manager, &[], &Cursor::of("c"), &mut one())
        .expect("read_after");

    assert_eq!(all[0].bytes("MESSAGE").map(<[u8]>::len), Some(4088));
}

#[skuld::test]
fn sync_runs_journalctl_dash_dash_sync() {
    // The script's own words say what it was asked.
    let e = sh(r#"echo "asked: $*" >&2; exit 3"#).sync().expect_err("it failed");
    let said = e.report().to_string();
    assert!(said.contains("asked: --sync"), "{said}");

    sh("exit 0").sync().expect("a sync that succeeded");
}

#[skuld::test]
fn a_journalctl_that_cannot_be_spawned_cannot_sync() {
    let journal = Journalctl::of(vec!["/nonexistent/goetia-journalctl".to_string()]);

    assert!(matches!(
        journal.sync().expect_err("no such program"),
        Error::Spawn { .. }
    ));
}

/// The environment every `systemctl` of goetia's gets: `SYSTEMD_COLORS=0` is one of its settings.
#[skuld::test]
fn journalctl_runs_with_the_environment_of_every_systemctl() {
    let journal = sh(r#"printf '{"COLORS":"%s"}\n' "$SYSTEMD_COLORS""#);

    let all = entries_of(&journal).expect("entries");

    assert_eq!(all[0].get("COLORS"), Some("0"));
}

/// A stopped follower gives what it had read and then ends without an error, though its child was
/// killed; the child is gone when the follower is dropped.
#[skuld::test]
fn a_stopped_follower_ends_with_no_error_and_leaves_no_child() {
    let journal = sh(r#"printf '{"PID":"%s"}\n' "$$"; exec sleep 100000"#);
    let mut follower = journal
        .follow(Scope::Manager, &[Match::new("UNIT", "x.service")], None, &mut one())
        .expect("spawn");
    let pid = follower
        .next()
        .expect("an entry")
        .expect("the first line")
        .get("PID")
        .expect("the child's pid")
        .to_string();
    let stop = follower.stop();

    stop();

    assert!(follower.next().expect("the end is no error").is_none());
    drop(follower);
    drop(stop);
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the journalctl outlived its follower"
    );
}

/// A follow does not end: a `journalctl` that exited by itself has lost the journal.
#[skuld::test]
fn a_follower_that_ends_on_its_own_is_the_error() {
    let journal = sh(r#"printf '{"N":"1"}\n'"#);
    let mut follower = journal
        .follow(Scope::Manager, &[Match::new("UNIT", "x.service")], None, &mut one())
        .expect("spawn");

    assert!(follower.next().expect("the entry").is_some());
    let e = follower.next().expect_err("it ended");

    assert!(e.report().to_string().contains("ended on its own"), "{e}");
}
