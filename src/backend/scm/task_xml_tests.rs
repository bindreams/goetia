use std::path::PathBuf;

use super::*;
use crate::backend::Identity;
use crate::backend::scm::generate::{WindowsDefinition, registration};
use crate::spec::test_support::{abs, program};
use crate::spec::{AccountId, DaemonSpec, TaskSpec, User};
use crate::testing::task;
use roxmltree::{Document, Node};

const ID: &str = "nightly";
const SCHEDULE: &str = "30 3 * * *";

// Fixtures ============================================================================================================

fn spec_of(user: User, schedule: Option<&str>) -> DaemonSpec {
    let DaemonSpec::Task(task) = task(ID, vec![program("job"), "--flag".to_owned()], schedule) else {
        unreachable!("`testing::task` builds a task")
    };
    let mut fields = task.into_fields();
    fields.common.user = user;
    TaskSpec::new(fields).map(DaemonSpec::from).unwrap()
}

fn spec() -> DaemonSpec {
    spec_of(User::Root, Some(SCHEDULE))
}

fn host_at(exe: impl Into<PathBuf>, args: &[&str]) -> HostCommand {
    HostCommand {
        exe: exe.into(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
    }
}

fn host() -> HostCommand {
    host_at(abs("Program Files/Goetia/goetia-shim.exe"), &["--service-host"])
}

fn identity() -> Identity {
    Identity { user: String::new() }
}

fn definition(spec: &DaemonSpec, host: HostCommand) -> Result<TaskXml, Error> {
    match registration(spec, &identity(), &|| Ok(host.clone()))? {
        WindowsDefinition::Task(task) => Ok(task),
        WindowsDefinition::Service(_) => panic!("a task spec registers as a task"),
    }
}

fn built(spec: &DaemonSpec) -> TaskXml {
    definition(spec, host()).unwrap()
}

fn invalid_message(error: Error) -> String {
    match error {
        Error::Invalid { message, .. } => message,
        other => panic!("expected Error::Invalid, got {other:?}"),
    }
}

/// The text at `path` from the root element (`Task/Settings/Priority`), trimmed.
fn at(document: &Document<'_>, path: &str) -> Option<String> {
    node_at(document, path).map(|node| node.text().unwrap_or_default().trim().to_owned())
}

fn node_at<'a, 'i>(document: &'a Document<'i>, path: &str) -> Option<Node<'a, 'i>> {
    let mut names = path.split('/');
    let mut node = document.root_element();
    if names.next()? != node.tag_name().name() {
        return None;
    }
    for name in names {
        node = node
            .children()
            .find(|child| child.is_element() && child.tag_name().name() == name)?;
    }
    Some(node)
}

/// The names of the element children of `path`, in order.
fn names_under(document: &Document<'_>, path: &str) -> Vec<String> {
    node_at(document, path)
        .map(|node| {
            node.children()
                .filter(Node::is_element)
                .map(|child| child.tag_name().name().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// `xml` as Task Scheduler reads it back after registration (M4): the elements reordered, the
/// defaults it was given dropped, `URI` and `UseUnifiedSchedulingEngine` added, the principal's
/// account a SID, and `Data` as CDATA with CRLF line ends.
fn readback(spec: &DaemonSpec, host: &HostCommand, triggers: &str) -> String {
    let data = blob::metadata_text(spec).replace('\n', "\r\n");
    let exe = host.exe.to_string_lossy();
    let args = host.args.join(" ");
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <URI>\Goetia\{ID}</URI>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-18</UserId>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <UseUnifiedSchedulingEngine>true</UseUnifiedSchedulingEngine>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Data><![CDATA[{data}]]></Data>
  {triggers}
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>{args} {TASK_MODE_ARG} {ID}</Arguments>
    </Exec>
  </Actions>
</Task>"#
    )
}

const DAILY_AT_0330: &str = r#"<Triggers>
    <CalendarTrigger>
      <StartBoundary>2000-01-01T03:30:00</StartBoundary>
      <ScheduleByDay>
        <DaysInterval>1</DaysInterval>
      </ScheduleByDay>
    </CalendarTrigger>
  </Triggers>"#;

fn data_of(spec: &DaemonSpec) -> String {
    blob::metadata_text(spec)
}

// The action ==========================================================================================================

/// What Task Scheduler builds from `Command` and `Arguments` and a child reads (M9: the arguments
/// arrive verbatim after the quoted command), split as `CommandLineToArgvW` splits it.
fn argv_of(exe: &str, arguments: &str) -> Vec<String> {
    let line: Vec<u16> = format!("\"{exe}\" {arguments}").encode_utf16().collect();
    cosca::quote::windows::split_wide(&line)
        .unwrap()
        .into_iter()
        .map(|arg| String::from_utf16(&arg).unwrap())
        .collect()
}

#[skuld::test]
fn a_task_is_one_exec_action_of_the_host_in_task_mode() {
    let args = [
        "--plain",
        "with space",
        "a\"quote",
        r"trailing\",
        r"C:\Program Files\x\",
        r#"back\\slash and \"quoted\""#,
        "",
        "ünï",
    ];
    let exe = abs("Program Files/Goetia/goetia-shim.exe");
    let xml = definition(&spec(), host_at(&exe, &args)).unwrap().to_xml(true);
    let document = Document::parse(&xml).unwrap();

    assert_eq!(names_under(&document, "Task/Actions"), ["Exec"], "one action");
    assert_eq!(
        at(&document, "Task/Actions/Exec/Command").unwrap(),
        exe.to_string_lossy()
    );
    assert!(
        node_at(&document, "Task/Actions/Exec/WorkingDirectory").is_none(),
        "no WorkingDirectory: the shim applies the spec's cwd"
    );
    let mut expected = vec![exe.to_string_lossy().into_owned()];
    expected.extend(args.iter().map(|arg| (*arg).to_owned()));
    expected.extend([TASK_MODE_ARG.to_owned(), ID.to_owned()]);
    assert_eq!(
        argv_of(
            &exe.to_string_lossy(),
            &at(&document, "Task/Actions/Exec/Arguments").unwrap()
        ),
        expected
    );
}

#[skuld::test]
fn a_percent_in_the_host_exe_or_arguments_is_refused_naming_the_limit() {
    let in_exe = definition(&spec(), host_at(abs("100%/goetia-shim.exe"), &["--service-host"]));
    assert_eq!(
        invalid_message(in_exe.unwrap_err()),
        "`the shim host's path` contains `%`, which Task Scheduler expands in a task's arguments; a task's program \
         and arguments cannot contain `%`"
    );
    let in_args = definition(&spec(), host_at(abs("goetia-shim.exe"), &["--service-host", "%PATH%"]));
    assert_eq!(
        invalid_message(in_args.unwrap_err()),
        "`the shim host's arguments` contains `%`, which Task Scheduler expands in a task's arguments; a task's \
         program and arguments cannot contain `%`"
    );
}

// The blob ============================================================================================================

#[skuld::test]
fn the_blob_is_a_top_level_data_element_with_lf_only() {
    let xml = built(&spec()).to_xml(true);
    let document = Document::parse(&xml).unwrap();

    let data = at(&document, "Task/Data").expect("`Data` is a child of `Task`");
    assert!(node_at(&document, "Task/RegistrationInfo/Data").is_none(), "{xml}");
    assert!(!xml.contains('\r'), "no CR anywhere in what is registered");
    assert_eq!(data, data.trim(), "no outer whitespace");
    assert_eq!(data, blob::metadata_text(&spec()));
    assert_eq!(data.lines().count(), 4);
}

#[skuld::test]
fn the_blob_round_trips_through_extract_from_data() {
    let spec = spec();
    let xml = built(&spec).to_xml(true);
    let document = Document::parse(&xml).unwrap();
    let data = at(&document, "Task/Data").unwrap();

    let blob = extract_from_data(&data).unwrap().expect("goetia's own data");
    assert_eq!(blob.spec, spec);
    assert_eq!(blob.schema, blob::SCHEMA);
    assert_eq!(blob.version, crate::version());
    // The CRLF form Task Scheduler's `Xml` carries reads as the same text.
    assert_eq!(
        extract_from_data(&data.replace('\n', "\r\n")).unwrap().unwrap().spec,
        spec
    );
}

#[skuld::test]
fn data_that_is_not_goetias_is_foreign_and_a_marker_with_a_broken_blob_is_an_error() {
    assert_eq!(extract_from_data("").unwrap(), None);
    assert_eq!(extract_from_data("an administrator's note").unwrap(), None);
    let broken = blob::metadata_text(&spec()).replacen("Spec: ", "Spec: !", 1);
    assert!(matches!(extract_from_data(&broken), Err(Error::Blob { .. })));
}

// The triggers and the settings =======================================================================================

#[skuld::test]
fn the_never_trigger_is_a_one_time_trigger_at_the_fixed_past_boundary() {
    let spec = spec_of(User::Root, Some("0 0 30 2 *"));
    let xml = built(&spec).to_xml(true);
    let document = Document::parse(&xml).unwrap();

    assert_eq!(names_under(&document, "Task/Triggers"), ["TimeTrigger"]);
    assert_eq!(
        at(&document, "Task/Triggers/TimeTrigger/StartBoundary").unwrap(),
        "2000-01-01T00:00:00"
    );
    assert_eq!(NEVER_AT, "2000-01-01T00:00:00");
}

#[skuld::test]
fn the_design_settings_are_written() {
    let xml = built(&spec()).to_xml(true);
    let document = Document::parse(&xml).unwrap();

    for (path, value) in [
        ("Task/Settings/ExecutionTimeLimit", "PT0S"),
        ("Task/Settings/MultipleInstancesPolicy", "IgnoreNew"),
        ("Task/Settings/StartWhenAvailable", "true"),
        ("Task/Settings/DisallowStartIfOnBatteries", "false"),
        ("Task/Settings/StopIfGoingOnBatteries", "false"),
        ("Task/Settings/RunOnlyIfNetworkAvailable", "false"),
        ("Task/Settings/RunOnlyIfIdle", "false"),
        ("Task/Settings/WakeToRun", "false"),
        ("Task/Settings/IdleSettings/StopOnIdleEnd", "false"),
        ("Task/Settings/IdleSettings/RestartOnIdle", "false"),
        ("Task/Settings/Enabled", "true"),
        ("Task/Settings/Priority", "5"),
        ("Task/Principals/Principal/RunLevel", "HighestAvailable"),
    ] {
        assert_eq!(at(&document, path).as_deref(), Some(value), "{path}\n{xml}");
    }
}

#[skuld::test]
fn a_trigger_is_written_with_local_time_and_no_zone() {
    let xml = built(&spec_of(User::Root, Some("*/15 9-17 * 3-5 1-5"))).to_xml(true);
    let document = Document::parse(&xml).unwrap();

    let trigger = "Task/Triggers/CalendarTrigger";
    assert_eq!(
        at(&document, &format!("{trigger}/StartBoundary")).unwrap(),
        "2000-01-01T09:00:00"
    );
    assert_eq!(
        at(&document, &format!("{trigger}/Repetition/Interval")).unwrap(),
        "PT15M"
    );
    // 09:00 to 17:45, the last tick included.
    assert_eq!(
        at(&document, &format!("{trigger}/Repetition/Duration")).unwrap(),
        "PT525M"
    );
    assert!(
        node_at(&document, &format!("{trigger}/Repetition/StopAtDurationEnd")).is_none(),
        "a Duration includes its last tick, so StopAtDurationEnd stays off"
    );
    let weeks = format!("{trigger}/ScheduleByMonthDayOfWeek/Weeks");
    assert_eq!(names_under(&document, &weeks), ["Week"; 5]);
    let texts: Vec<_> = node_at(&document, &weeks)
        .unwrap()
        .children()
        .filter(Node::is_element)
        .map(|week| week.text().unwrap().to_owned())
        .collect();
    assert_eq!(texts, ["1", "2", "3", "4", "Last"], "a fifth week is never written");
    assert_eq!(
        names_under(&document, &format!("{trigger}/ScheduleByMonthDayOfWeek/Months")),
        ["March", "April", "May"]
    );
    assert_eq!(
        names_under(&document, &format!("{trigger}/ScheduleByMonthDayOfWeek/DaysOfWeek")),
        ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday"]
    );
    assert!(
        !xml.contains("T09:00:00Z") && !xml.contains("T09:00:00+"),
        "no zone suffix"
    );
}

// The principal =======================================================================================================

#[skuld::test]
fn each_account_maps_to_its_principal() {
    let services = [
        (User::Root, "S-1-5-18"),
        (User::Name("SYSTEM".into()), "S-1-5-18"),
        (User::Name("LocalService".into()), "S-1-5-19"),
        (User::Name(r"NT AUTHORITY\LOCAL SERVICE".into()), "S-1-5-19"),
        (User::Name(r"NT AUTHORITY\LocalService".into()), "S-1-5-19"),
        (User::Name("NetworkService".into()), "S-1-5-20"),
        (User::Name(r"NT AUTHORITY\NETWORK SERVICE".into()), "S-1-5-20"),
        (User::Name(r"NT AUTHORITY\NetworkService".into()), "S-1-5-20"),
        (User::Id(AccountId::Sid("S-1-5-19".into())), "S-1-5-19"),
    ];
    for (user, sid) in services {
        let task = built(&spec_of(user.clone(), Some(SCHEDULE)));
        let xml = task.to_xml(true);
        let document = Document::parse(&xml).unwrap();
        assert_eq!(
            at(&document, "Task/Principals/Principal/UserId").as_deref(),
            Some(sid),
            "{user:?}"
        );
        assert!(
            node_at(&document, "Task/Principals/Principal/LogonType").is_none(),
            "a service account has no LogonType: {user:?}"
        );
        assert_eq!(task.account(), TaskAccount::Service(sid.to_owned()), "{user:?}");
    }

    let task = built(&spec_of(User::Name("alice".into()), Some(SCHEDULE)));
    let xml = task.to_xml(true);
    let document = Document::parse(&xml).unwrap();
    assert_eq!(
        at(&document, "Task/Principals/Principal/UserId").as_deref(),
        Some("alice")
    );
    assert_eq!(
        at(&document, "Task/Principals/Principal/LogonType").as_deref(),
        Some("Password")
    );
    assert_eq!(task.account(), TaskAccount::User("alice".to_owned()));
}

#[skuld::test]
fn a_virtual_account_is_refused_naming_the_limit() {
    for account in [r"NT SERVICE\frpc", r"nt service\Spooler"] {
        let error = definition(&spec_of(User::Name(account.into()), Some(SCHEDULE)), host()).unwrap_err();
        assert_eq!(
            invalid_message(error),
            format!(
                "`{account}` is a virtual service account, and Task Scheduler runs a task registered with one as \
                 LOCAL SERVICE, so a task cannot run as it; use `LocalService`, `NetworkService`, `root` or a user \
                 account"
            )
        );
    }
}

// The readback ========================================================================================================

#[skuld::test]
fn parse_of_task_scheduler_normalised_xml_is_the_definition() {
    for (user, schedule) in [
        (User::Root, Some(SCHEDULE)),
        (
            User::Id(AccountId::Sid("S-1-5-21-1-2-3-1001".into())),
            Some("*/15 9-17 * 3-5 1-5"),
        ),
        (User::Root, Some("0 0 1,15 * *")),
        (User::Root, Some("0 0 * * 0,6")),
        (User::Root, Some("0 0 30 2 *")),
    ] {
        let spec = spec_of(user, schedule);
        let desired = built(&spec);
        // The readback differs from what was written in every way M4 measured; only the triggers are
        // taken from what was written, since a literal for each shape would restate the writer.
        let written = desired.to_xml(true);
        let triggers = {
            let document = Document::parse(&written).unwrap();
            node_at(&document, "Task/Triggers").map(|node| written[node.range()].to_owned())
        };
        // Task Scheduler leaves out a flag that is the default.
        let triggers = triggers
            .expect("a schedule has triggers")
            .replace("<Enabled>true</Enabled>", "");
        let installed = TaskXml::parse(&readback(&spec, &host(), &triggers), &data_of(&spec)).unwrap();

        assert_eq!(installed.to_string(), desired.to_string(), "{schedule:?}\n{triggers}");
        // `generate(extract(artifact)) == artifact`, by `Display`.
        let blob = extract_from_data(&data_of(&spec)).unwrap().unwrap();
        let regenerated = definition(&blob.spec, installed.host()).unwrap();
        assert_eq!(regenerated.to_string(), installed.to_string());
        assert_eq!(installed.host(), host());
    }
}

#[skuld::test]
fn the_account_is_the_blobs_and_never_read_from_the_principal() {
    let spec = spec_of(User::Name("alice".into()), Some(SCHEDULE));
    // Task Scheduler rewrites the account to a SID, which is no name the spec holds.
    let xml = readback(&spec, &host(), DAILY_AT_0330).replace(
        "<UserId>S-1-5-18</UserId>",
        "<UserId>S-1-5-21-1-2-3-1001</UserId><LogonType>Password</LogonType>",
    );
    let installed = TaskXml::parse(&xml, &data_of(&spec)).unwrap();
    assert_eq!(installed.account(), TaskAccount::User("alice".to_owned()));
    assert_eq!(installed.to_string(), built(&spec).to_string());
}

#[skuld::test]
fn a_readback_is_a_fixed_point() {
    let spec = spec();
    let first = TaskXml::parse(&readback(&spec, &host(), DAILY_AT_0330), &data_of(&spec)).unwrap();
    // Rendered with a trigger the readback dropped the flag of, then read again.
    let again = TaskXml::parse(&first.to_xml(true), &data_of(&spec)).unwrap();
    assert_eq!(first, again);
    assert_eq!(first.to_string(), again.to_string());
    let disarmed = TaskXml::parse(&first.to_xml(false), &data_of(&spec)).unwrap();
    assert_eq!(first, disarmed, "the flags are not part of the definition");
}

#[skuld::test]
fn a_hand_edit_of_a_setting_trigger_or_action_is_a_difference() {
    let spec = spec();
    let desired = built(&spec).to_string();
    let base = readback(&spec, &host(), DAILY_AT_0330);
    for (what, edited) in [
        (
            "priority",
            base.replace("<Priority>5</Priority>", "<Priority>7</Priority>"),
        ),
        ("time limit", base.replace("PT0S", "PT72H")),
        ("hidden", base.replace("<Priority>", "<Hidden>true</Hidden><Priority>")),
        ("trigger time", base.replace("T03:30:00", "T04:30:00")),
        ("interval", base.replace("<DaysInterval>1", "<DaysInterval>2")),
        (
            "a second trigger",
            base.replace(
                "</Triggers>",
                "<LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>",
            ),
        ),
        ("the action's arguments", base.replace(TASK_MODE_ARG, "--other")),
        (
            "a working directory",
            base.replace("</Exec>", "<WorkingDirectory>C:\\x</WorkingDirectory></Exec>"),
        ),
    ] {
        let installed = TaskXml::parse(&edited, &data_of(&spec)).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_ne!(installed.to_string(), desired, "{what}");
    }
}

#[skuld::test]
fn parse_refuses_data_that_is_not_a_tasks_blob() {
    let service = crate::testing::base_fields("frpc", vec![program("frpc")]);
    let service = DaemonSpec::from(crate::spec::ServiceSpec::new(service).unwrap());
    let xml = readback(&spec(), &host(), DAILY_AT_0330);
    for data in [String::new(), "a note".to_owned(), data_of(&service)] {
        assert!(TaskXml::parse(&xml, &data).is_err(), "{data:?}");
    }
}

// Arming ==============================================================================================================

/// `DAILY_AT_0330` and a second trigger with its flag stated.
const TWO_TRIGGERS: &str = r#"<Triggers>
    <CalendarTrigger>
      <StartBoundary>2000-01-01T03:30:00</StartBoundary>
      <Enabled>false</Enabled>
      <ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>
    </CalendarTrigger>
    <TimeTrigger>
      <StartBoundary>2000-01-01T00:00:00</StartBoundary>
    </TimeTrigger>
  </Triggers>"#;

/// `xml` with every `Enabled` element removed: in a readback, only triggers state one.
fn without_flags(xml: &str) -> String {
    let mut out = String::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Enabled>") {
        let (before, after) = rest.split_at(start);
        out.push_str(before);
        rest = &after[after.find("</Enabled>").unwrap() + "</Enabled>".len()..];
    }
    out.push_str(rest);
    out
}

#[skuld::test]
fn arming_flips_only_the_trigger_flags() {
    let spec = spec();
    let xml = readback(&spec, &host(), TWO_TRIGGERS);
    assert!(!TaskXml::armed(&xml), "one trigger is disabled");

    let armed = TaskXml::with_armed(&xml, true).unwrap();
    assert!(TaskXml::armed(&armed));
    assert_eq!(
        without_flags(&armed),
        without_flags(&xml),
        "nothing but the flags changed"
    );

    let disarmed = TaskXml::with_armed(&armed, false).unwrap();
    assert!(!TaskXml::armed(&disarmed));
    assert_eq!(without_flags(&disarmed), without_flags(&xml));
    let document = Document::parse(&disarmed).unwrap();
    let flags: Vec<_> = node_at(&document, "Task/Triggers")
        .unwrap()
        .children()
        .filter(Node::is_element)
        .map(|trigger| {
            trigger
                .children()
                .find(|child| child.tag_name().name() == "Enabled")
                .map(|flag| flag.text().unwrap().to_owned())
        })
        .collect();
    assert_eq!(
        flags,
        [Some("false".to_owned()), Some("false".to_owned())],
        "a dropped flag is stated"
    );

    // A hand edit made elsewhere survives the flip.
    let edited = xml.replace("<Priority>5</Priority>", "<Priority>3</Priority>");
    assert!(
        TaskXml::with_armed(&edited, true)
            .unwrap()
            .contains("<Priority>3</Priority>")
    );
}

#[skuld::test]
fn armed_flags_are_not_in_display() {
    let spec = spec();
    let xml = readback(&spec, &host(), DAILY_AT_0330);
    let armed = TaskXml::parse(&TaskXml::with_armed(&xml, true).unwrap(), &data_of(&spec)).unwrap();
    let disarmed = TaskXml::parse(&TaskXml::with_armed(&xml, false).unwrap(), &data_of(&spec)).unwrap();
    assert_eq!(armed.to_string(), disarmed.to_string());
    let desired = built(&spec);
    assert_eq!(desired.to_string(), armed.to_string());
    assert!(desired.to_xml(true).contains("<Enabled>true</Enabled>"));
    assert!(desired.to_xml(false).contains("<Enabled>false</Enabled>"));
    assert!(TaskXml::armed(&desired.to_xml(true)));
    assert!(!TaskXml::armed(&desired.to_xml(false)));
}

#[skuld::test]
fn a_task_with_no_schedule_has_no_triggers_and_is_never_armed() {
    let spec = spec_of(User::Root, None);
    let task = built(&spec);
    for armed in [true, false] {
        let xml = task.to_xml(armed);
        let document = Document::parse(&xml).unwrap();
        assert!(
            node_at(&document, "Task/Triggers").is_none_or(|node| node.children().all(|child| !child.is_element())),
            "{xml}"
        );
        assert!(!TaskXml::armed(&xml), "no trigger is not armed");
        assert_eq!(TaskXml::with_armed(&xml, true).unwrap(), xml, "nothing to flip");
    }
    // `Triggers` may read back as an empty element.
    let empty = readback(&spec, &host(), "<Triggers />");
    assert!(!TaskXml::armed(&empty));
    let installed = TaskXml::parse(&empty, &data_of(&spec)).unwrap();
    assert_eq!(installed.to_string(), task.to_string());
}

/// Triggers goetia does not write, added by hand: neither has a `StartBoundary` or an `Enabled`.
const HAND_ADDED: &str = r#"<Triggers>
    <CalendarTrigger>
      <StartBoundary>2000-01-01T03:30:00</StartBoundary>
      <ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>
    </CalendarTrigger>
    <LogonTrigger>
      <UserId>S-1-5-21-1-2-3-1001</UserId>
    </LogonTrigger>
    <BootTrigger />
  </Triggers>"#;

#[skuld::test]
fn a_trigger_with_no_start_boundary_is_armed_and_disarmed_like_any_other() {
    let spec = spec();
    let xml = readback(&spec, &host(), HAND_ADDED);
    assert!(TaskXml::armed(&xml), "no flag is enabled");

    let disarmed = TaskXml::with_armed(&xml, false).expect("a hand-added trigger does not stop `disable`");
    assert!(!TaskXml::armed(&disarmed));
    let document = Document::parse(&disarmed).unwrap();
    let flags: Vec<_> = node_at(&document, "Task/Triggers")
        .unwrap()
        .children()
        .filter(Node::is_element)
        .map(|trigger| {
            let flag = trigger.children().find(|child| child.tag_name().name() == "Enabled");
            (
                trigger.tag_name().name().to_owned(),
                flag.map(|flag| flag.text().unwrap().to_owned()),
            )
        })
        .collect();
    assert_eq!(
        flags,
        [
            ("CalendarTrigger".to_owned(), Some("false".to_owned())),
            ("LogonTrigger".to_owned(), Some("false".to_owned())),
            ("BootTrigger".to_owned(), Some("false".to_owned())),
        ]
    );
    assert!(
        disarmed.contains("<UserId>S-1-5-21-1-2-3-1001</UserId>"),
        "the hand edit is kept"
    );

    let armed = TaskXml::with_armed(&disarmed, true).unwrap();
    assert!(TaskXml::armed(&armed));
    // The hand-added triggers are drift, whatever their flags.
    let installed = TaskXml::parse(&armed, &data_of(&spec)).unwrap();
    assert!(installed.to_string().contains("Unrecognised:"), "{installed}");
}

#[skuld::test]
fn a_repetition_trigger_reads_back_in_task_schedulers_order() {
    // Task Scheduler's order inside the schedule: `Months` before `Weeks` and `DaysOfWeek`; a duration
    // it kept as it was given, in hours and minutes.
    let spec = spec_of(User::Root, Some("*/15 9-17 * 3-5 1-5"));
    let triggers = r#"<Triggers>
    <CalendarTrigger>
      <StartBoundary>2000-01-01T09:00:00</StartBoundary>
      <Repetition>
        <Interval>PT15M</Interval>
        <Duration>PT8H45M</Duration>
      </Repetition>
      <ScheduleByMonthDayOfWeek>
        <Months><March /><April /><May /></Months>
        <Weeks><Week>1</Week><Week>2</Week><Week>3</Week><Week>4</Week><Week>Last</Week></Weeks>
        <DaysOfWeek><Monday /><Tuesday /><Wednesday /><Thursday /><Friday /></DaysOfWeek>
      </ScheduleByMonthDayOfWeek>
    </CalendarTrigger>
  </Triggers>"#;
    let installed = TaskXml::parse(&readback(&spec, &host(), triggers), &data_of(&spec)).unwrap();
    assert_eq!(installed.to_string(), built(&spec).to_string());
    assert!(!installed.to_string().contains("Unrecognised"), "{installed}");
}

#[skuld::test]
fn a_schedule_that_never_fires_has_a_trigger_and_can_be_armed() {
    let spec = spec_of(User::Root, Some("0 0 31 4,6 *"));
    let task = built(&spec);
    assert!(!TaskXml::armed(&task.to_xml(false)));
    assert!(TaskXml::armed(&task.to_xml(true)));
    let armed = TaskXml::with_armed(&task.to_xml(false), true).unwrap();
    assert!(TaskXml::armed(&armed));
}

// The generator's entry ===============================================================================================

#[skuld::test]
fn a_native_service_never_asks_for_a_host_and_a_task_always_does() {
    let native = DaemonSpec::from(
        crate::spec::ServiceSpec::new(crate::testing::base_fields("frpc", vec![program("frpc")])).unwrap(),
    );
    let never = || -> Result<HostCommand, Error> { panic!("a native service never asks for a host") };
    registration(&native, &identity(), &never).unwrap();
    crate::backend::scm::generate::install_registration(&native, &identity(), &never).unwrap();

    let asked = std::cell::Cell::new(0);
    let counting = || -> Result<HostCommand, Error> {
        asked.set(asked.get() + 1);
        Ok(host())
    };
    registration(&spec(), &identity(), &counting).unwrap();
    assert_eq!(asked.get(), 1, "a task asks once");
    crate::backend::scm::generate::install_registration(&spec(), &identity(), &counting).unwrap();
    assert_eq!(asked.get(), 2, "and so does its install");
    let failing = || -> Result<HostCommand, Error> { Err(Error::NoShimProvider) };
    assert!(matches!(
        registration(&spec(), &identity(), &failing),
        Err(Error::NoShimProvider)
    ));
}

#[skuld::test]
fn a_windows_definition_names_its_task_host() {
    let task = registration(&spec(), &identity(), &|| Ok(host())).unwrap();
    assert_eq!(task.host(), Some(host()));
    assert_eq!(task.to_string(), built(&spec()).to_string(), "Display delegates");
    let native = DaemonSpec::from(
        crate::spec::ServiceSpec::new(crate::testing::base_fields("frpc", vec![program("frpc")])).unwrap(),
    );
    let service = registration(&native, &identity(), &|| Ok(host())).unwrap();
    assert_eq!(service.host(), None);
}
