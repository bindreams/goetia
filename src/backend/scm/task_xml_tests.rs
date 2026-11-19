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
    let xml = built(&spec_of(User::Root, Some("15,30,45 9 * 3-5 1-5"))).to_xml(true);
    let document = Document::parse(&xml).unwrap();

    let trigger = "Task/Triggers/CalendarTrigger";
    assert_eq!(
        at(&document, &format!("{trigger}/StartBoundary")).unwrap(),
        "2000-01-01T09:15:00"
    );
    assert_eq!(
        at(&document, &format!("{trigger}/Repetition/Interval")).unwrap(),
        "PT15M"
    );
    // 09:15 to 09:45, the last tick included.
    assert_eq!(
        at(&document, &format!("{trigger}/Repetition/Duration")).unwrap(),
        "PT30M"
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
        !xml.contains("T09:15:00Z") && !xml.contains("T09:15:00+"),
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

/// A task whose blob cannot be read still has a principal, which is how it is registered again (an
/// `uninstall` that disarms it first): a password logon is a user's, anything else a service account.
#[skuld::test]
fn a_stored_principal_names_the_account_to_register_again_as() {
    let service = built(&spec()).to_xml(true);
    assert_eq!(
        TaskXml::principal(&service).unwrap(),
        TaskAccount::Service("S-1-5-18".to_owned())
    );

    let user = built(&spec_of(User::Name("alice".into()), Some(SCHEDULE))).to_xml(true);
    assert_eq!(
        TaskXml::principal(&user).unwrap(),
        TaskAccount::User("alice".to_owned())
    );

    // As Task Scheduler reads it back: the account a SID.
    let normalised = user.replace("<UserId>alice</UserId>", "<UserId>S-1-5-21-1-2-3-1001</UserId>");
    assert_eq!(
        TaskXml::principal(&normalised).unwrap(),
        TaskAccount::User("S-1-5-21-1-2-3-1001".to_owned())
    );

    let none = "<Task xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\"><Actions/></Task>";
    assert!(TaskXml::principal(none).is_err());
    assert!(TaskXml::principal("<Task><Principals>").is_err());
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
    assert_eq!(first, disarmed, "a disarmed task reads as the definition it would arm");
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

/// A stored trigger whose own flag reads false is not goetia's, so it is a difference (an install writes
/// the desired one over it); one that reads true is.
#[skuld::test]
fn a_trigger_whose_flag_is_false_is_a_difference_and_one_whose_flag_is_true_is_not() {
    let spec = spec();
    let desired = built(&spec).to_string();
    let base = readback(&spec, &host(), DAILY_AT_0330);
    let boundary = "<StartBoundary>2000-01-01T03:30:00</StartBoundary>";
    for flag in ["false", "False", "0"] {
        let edited = base.replace(boundary, &format!("{boundary}<Enabled>{flag}</Enabled>"));
        let installed = TaskXml::parse(&edited, &data_of(&spec)).unwrap();
        assert_ne!(installed.to_string(), desired, "{flag}");
    }
    let edited = base.replace(boundary, &format!("{boundary}<Enabled>true</Enabled>"));
    let installed = TaskXml::parse(&edited, &data_of(&spec)).unwrap();
    assert_eq!(installed.to_string(), desired);
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

/// `xml` with its `Triggers` element, empty or not, taken out: what a disarmed task stores. XML with
/// none is as it was.
fn without_triggers_element(xml: &str) -> String {
    let Some(start) = xml.find("<Triggers") else {
        return xml.to_owned();
    };
    let end = match xml[start..].find("</Triggers>") {
        Some(close) => start + close + "</Triggers>".len(),
        None => start + xml[start..].find("/>").expect("an empty element closes itself") + 2,
    };
    format!("{}{}", &xml[..start], &xml[end..])
}

fn has_triggers_element(xml: &str) -> bool {
    node_at(&Document::parse(xml).unwrap(), "Task/Triggers").is_some()
}

/// Disarming takes out the whole `Triggers` element, whatever it holds (a hand-added trigger included),
/// and changes nothing else.
#[skuld::test]
fn disarming_removes_the_triggers_and_nothing_else() {
    let spec = spec();
    for triggers in [DAILY_AT_0330, HAND_ADDED, "<Triggers />"] {
        let xml = readback(&spec, &host(), triggers);
        let disarmed = TaskXml::without_triggers(&xml).unwrap();
        assert_eq!(disarmed, without_triggers_element(&xml), "{triggers}");
        assert!(!has_triggers_element(&disarmed), "{triggers}");
        assert!(!TaskXml::armed(&disarmed), "{triggers}");
    }

    // A hand edit made elsewhere survives.
    let edited = readback(&spec, &host(), DAILY_AT_0330).replace("<Priority>5</Priority>", "<Priority>3</Priority>");
    assert!(
        TaskXml::without_triggers(&edited)
            .unwrap()
            .contains("<Priority>3</Priority>")
    );

    // A task with none is as it was.
    let none = readback(&spec, &host(), "");
    assert_eq!(TaskXml::without_triggers(&none).unwrap(), none);
}

/// Arming writes the triggers the blob's schedule compiles to, in place of whatever the stored XML holds,
/// and changes nothing else.
#[skuld::test]
fn arming_writes_the_blobs_triggers_into_the_stored_xml() {
    let spec = spec();
    let desired = built(&spec).to_string();
    for stored in ["", "<Triggers />", HAND_ADDED, DAILY_AT_0330] {
        let xml = readback(&spec, &host(), stored);
        let armed = TaskXml::with_triggers(&xml, &spec).unwrap();

        assert!(TaskXml::armed(&armed), "{stored}: {armed}");
        let installed = TaskXml::parse(&armed, &data_of(&spec)).unwrap();
        assert_eq!(
            installed.to_string(),
            desired,
            "{stored}: the spec's triggers, and no other"
        );
        let document = Document::parse(&armed).unwrap();
        let children = names_under(&document, "Task");
        let at = |name: &str| children.iter().position(|child| child == name).unwrap();
        assert_eq!(
            children.iter().filter(|child| *child == "Triggers").count(),
            1,
            "{stored}"
        );
        assert!(at("Triggers") < at("Actions"), "{stored}: {children:?}");
        let squashed = |xml: &str| without_triggers_element(xml).split_whitespace().collect::<String>();
        assert_eq!(
            squashed(&armed),
            squashed(&xml),
            "{stored}: nothing but the triggers differs"
        );
    }

    // A hand edit made elsewhere survives.
    let edited = readback(&spec, &host(), "").replace("<Priority>5</Priority>", "<Priority>3</Priority>");
    assert!(
        TaskXml::with_triggers(&edited, &spec)
            .unwrap()
            .contains("<Priority>3</Priority>")
    );
}

/// A task whose triggers are there is armed by its own flag alone: they are kept as they are, a hand-added
/// one included, and nothing else changes.
#[skuld::test]
fn keeping_the_triggers_sets_the_own_flag_and_changes_nothing_else() {
    let spec = spec();
    for stored in [DAILY_AT_0330, HAND_ADDED] {
        let xml = readback(&spec, &host(), stored);
        assert_eq!(
            TaskXml::with_own_flag(&xml, true).unwrap(),
            xml,
            "{stored}: already true"
        );

        let off = with_own_flag(&xml, false);
        let on = TaskXml::with_own_flag(&off, true).unwrap();
        assert_eq!(on, with_own_flag(&xml, true), "{stored}");
        assert!(TaskXml::armed(&on), "{stored}");
    }
}

/// A false own flag is stated once whether the XML states a flag or not, and nothing else changes: setting it
/// true again gives back what a flag stated true would be.
#[skuld::test]
fn setting_the_own_flag_false_states_it_once_and_changes_nothing_else() {
    let spec = spec();
    let xml = readback(&spec, &host(), DAILY_AT_0330);
    let off = TaskXml::with_own_flag(&xml, false).unwrap();
    assert_eq!(off.matches("<Enabled>false</Enabled>").count(), 1, "{off}");
    assert!(!TaskXml::armed(&off));
    assert_eq!(
        TaskXml::with_own_flag(&off, true)
            .unwrap()
            .replace("<Enabled>true</Enabled>", ""),
        xml
    );

    let stated = with_own_flag(&xml, true);
    let off = TaskXml::with_own_flag(&stated, false).unwrap();
    assert_eq!(off.matches("<Enabled>").count(), 1, "{off}");
    assert_eq!(off, with_own_flag(&xml, false));
}

/// A false own flag is written whether the `Settings` is empty or missing, and the XML is no less goetia's for it.
#[skuld::test]
fn setting_the_own_flag_false_writes_the_settings_it_needs() {
    let xml = readback(&spec(), &host(), DAILY_AT_0330);
    let start = xml.find("<Settings>").unwrap();
    let end = xml.find("</Settings>").unwrap() + "</Settings>".len();
    for (what, settings) in [("empty", "<Settings />"), ("none", "")] {
        let xml = format!("{}{settings}{}", &xml[..start], &xml[end..]);
        let off = TaskXml::with_own_flag(&xml, false).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(off.matches("<Enabled>false</Enabled>").count(), 1, "{what}: {off}");
        assert!(!TaskXml::armed(&off), "{what}: {off}");
        assert!(
            off.find("<Settings>").unwrap() < off.find("<Actions").unwrap(),
            "{what}: {off}"
        );
        assert!(TaskXml::armed(&TaskXml::with_own_flag(&off, true).unwrap()), "{what}");
    }
}

/// A trigger that is switched off starts nothing, so a task whose only triggers are off is not armed, and
/// arming it writes the blob's schedule in place of them.
#[skuld::test]
fn a_task_whose_only_triggers_are_off_is_not_armed_and_is_armed_with_fresh_ones() {
    let spec = spec();
    let off = DAILY_AT_0330.replace("<CalendarTrigger>", "<CalendarTrigger>\n      <Enabled>false</Enabled>");
    assert_ne!(off, DAILY_AT_0330);
    let xml = readback(&spec, &host(), &off);
    assert!(!TaskXml::has_triggers(&xml), "{xml}");
    assert!(!TaskXml::armed(&xml), "{xml}");

    let armed = TaskXml::with_triggers(&xml, &spec).unwrap();
    assert!(TaskXml::armed(&armed), "{armed}");
    assert!(!armed.contains("<Enabled>false</Enabled>"), "{armed}");

    // One that is on among them is enough.
    let mixed = readback(
        &spec,
        &host(),
        &off.replace("</Triggers>", "<BootTrigger /></Triggers>"),
    );
    assert!(TaskXml::armed(&mixed), "{mixed}");
}

/// A blob that holds a service names no schedule, so arming from it is an error, not a panic.
#[skuld::test]
fn arming_from_a_services_blob_is_an_error_that_names_it() {
    let service = crate::testing::base_fields("frpc", vec![program("frpc")]);
    let service = DaemonSpec::from(crate::spec::ServiceSpec::new(service).unwrap());
    let xml = readback(&spec(), &host(), "");
    let refused = TaskXml::with_triggers(&xml, &service).unwrap_err();
    assert!(refused.to_string().contains("service"), "{refused}");
}

/// A task with no schedule has nothing to arm: the triggers of its blob are none.
#[skuld::test]
fn arming_a_task_with_no_schedule_is_refused() {
    let spec = spec_of(User::Root, None);
    let xml = readback(&spec, &host(), "");
    let refused = TaskXml::with_triggers(&xml, &spec).unwrap_err();
    assert!(
        refused.to_string().contains(crate::manager::flow::NOTHING_TO_ARM),
        "{refused}"
    );
}

/// `xml` with the task's own flag stated as `flag` in its `Settings`, as Task Scheduler reads a task back
/// whose own flag is not the default (it drops the default, true).
fn with_own_flag(xml: &str, flag: bool) -> String {
    xml.replace("<Settings>", &format!("<Settings>\n    <Enabled>{flag}</Enabled>"))
}

/// A task is armed when its own flag is true and it has a trigger. A task whose own flag is false is
/// started by nothing, whatever its triggers say (an `uninstall` that did not finish leaves one), and one with
/// no trigger has nothing to start it.
#[skuld::test]
fn armed_is_the_own_flag_true_and_a_trigger_present() {
    let spec = spec();
    let with_trigger = readback(&spec, &host(), DAILY_AT_0330);
    assert!(TaskXml::armed(&with_trigger));
    assert!(!TaskXml::armed(&with_own_flag(&with_trigger, false)));
    assert!(TaskXml::armed(&with_own_flag(&with_trigger, true)));

    for none in ["", "<Triggers />"] {
        let xml = readback(&spec, &host(), none);
        assert!(!TaskXml::armed(&xml), "{none:?}");
        assert!(!TaskXml::armed(&with_own_flag(&xml, true)), "{none:?}");
    }
}

/// Arming and disarming both leave the task's own flag true: `enable` makes a task whose flag an
/// unfinished `uninstall` cleared armed, and `disable` leaves it disarmed but able to be started.
#[skuld::test]
fn arming_and_disarming_both_set_the_tasks_own_flag() {
    let spec = spec();
    let off = with_own_flag(&readback(&spec, &host(), DAILY_AT_0330), false);

    let armed = TaskXml::with_triggers(&off, &spec).unwrap();
    assert!(TaskXml::armed(&armed), "{armed}");
    assert!(
        armed.contains("<Settings>\n    <Enabled>true</Enabled>"),
        "the task's own flag is true: {armed}"
    );
    let disarmed = TaskXml::without_triggers(&off).unwrap();
    assert!(!TaskXml::armed(&disarmed));
    assert!(
        disarmed.contains("<Settings>\n    <Enabled>true</Enabled>"),
        "the task's own flag is true, and it has no trigger: {disarmed}"
    );

    // A task with no trigger has only its own flag to set.
    let no_triggers = readback(&spec, &host(), "");
    assert_eq!(
        TaskXml::without_triggers(&with_own_flag(&no_triggers, false)).unwrap(),
        with_own_flag(&no_triggers, true)
    );
}

/// What a disarmed task stores has no triggers, and still reads as the definition an install of its spec
/// writes: an install finds it up to date, and writes no trigger over it. A disarmed task whose blob names
/// another schedule differs.
#[skuld::test]
fn a_disarmed_task_reads_as_the_definition_it_would_arm() {
    let spec = spec();
    let desired = built(&spec);
    let disarmed_xml = TaskXml::without_triggers(&readback(&spec, &host(), DAILY_AT_0330)).unwrap();

    let installed = TaskXml::parse(&disarmed_xml, &data_of(&spec)).unwrap();
    assert_eq!(installed.to_string(), desired.to_string());
    let from_to_xml = TaskXml::parse(&desired.to_xml(false), &data_of(&spec)).unwrap();
    assert_eq!(from_to_xml.to_string(), desired.to_string());

    let moved = spec_of(User::Root, Some("0 4 * * *"));
    assert_ne!(
        TaskXml::parse(&disarmed_xml, &data_of(&moved)).unwrap().to_string(),
        desired.to_string()
    );
    assert_ne!(
        built(&moved).to_string(),
        desired.to_string(),
        "an install of the new schedule is a difference"
    );
}

/// `to_xml` writes the triggers only for an armed task: an install over a disarmed task keeps it so.
#[skuld::test]
fn to_xml_writes_triggers_only_when_armed() {
    let desired = built(&spec());
    let armed = desired.to_xml(true);
    let disarmed = desired.to_xml(false);
    assert!(has_triggers_element(&armed));
    assert!(TaskXml::armed(&armed));
    assert!(!has_triggers_element(&disarmed), "{disarmed}");
    assert!(!TaskXml::armed(&disarmed));
    assert!(
        !disarmed.contains("<Enabled>false</Enabled>"),
        "no trigger flag is written: {disarmed}"
    );
    assert_eq!(
        armed.split_whitespace().collect::<String>(),
        {
            let triggers = &armed[armed.find("<Triggers>").unwrap()..armed.find("</Triggers>").unwrap() + 11];
            disarmed
                .replace("<Actions", &format!("{triggers}<Actions"))
                .split_whitespace()
                .collect::<String>()
        },
        "the two differ by the triggers alone"
    );
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
        assert_eq!(TaskXml::without_triggers(&xml).unwrap(), xml, "nothing to take out");
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

/// Triggers goetia does not write are armed triggers and drift; `disable` takes them out with the rest, and
/// the next `enable` writes the spec's own.
#[skuld::test]
fn a_hand_added_trigger_is_armed_and_drift_until_a_disable() {
    let spec = spec();
    let xml = readback(&spec, &host(), HAND_ADDED);
    assert!(TaskXml::armed(&xml));
    let installed = TaskXml::parse(&xml, &data_of(&spec)).unwrap();
    assert!(installed.to_string().contains("Unrecognised:"), "{installed}");

    let disarmed = TaskXml::without_triggers(&xml).unwrap();
    assert!(
        !disarmed.contains("S-1-5-21-1-2-3-1001"),
        "the hand edit goes at `disable`"
    );
    let installed = TaskXml::parse(&disarmed, &data_of(&spec)).unwrap();
    assert!(!installed.to_string().contains("Unrecognised:"), "{installed}");
}

#[skuld::test]
fn a_repetition_trigger_reads_back_in_task_schedulers_order() {
    // Task Scheduler's order inside the schedule: `Months` before `Weeks` and `DaysOfWeek`.
    let spec = spec_of(User::Root, Some("15,30,45 9 * 3-5 1-5"));
    let triggers = r#"<Triggers>
    <CalendarTrigger>
      <StartBoundary>2000-01-01T09:15:00</StartBoundary>
      <Repetition>
        <Interval>PT15M</Interval>
        <Duration>PT30M</Duration>
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

/// A group that names minute 0 compiles to a lone `:00` trigger and a repetition from the next minute in
/// the same hour: both are written, and Task Scheduler's readback of them is the task that was built.
#[skuld::test]
fn a_lone_zero_trigger_and_a_repetition_in_one_hour_read_back_unchanged() {
    let spec = spec_of(User::Root, Some("*/15 9 * * *"));
    let task = built(&spec);
    let xml = task.to_xml(true);
    let document = Document::parse(&xml).unwrap();
    assert_eq!(
        names_under(&document, "Task/Triggers"),
        ["CalendarTrigger", "CalendarTrigger"]
    );
    let triggers = r#"<Triggers>
    <CalendarTrigger>
      <StartBoundary>2000-01-01T09:00:00</StartBoundary>
      <ScheduleByDay>
        <DaysInterval>1</DaysInterval>
      </ScheduleByDay>
    </CalendarTrigger>
    <CalendarTrigger>
      <StartBoundary>2000-01-01T09:15:00</StartBoundary>
      <Repetition>
        <Interval>PT15M</Interval>
        <Duration>PT30M</Duration>
      </Repetition>
      <ScheduleByDay>
        <DaysInterval>1</DaysInterval>
      </ScheduleByDay>
    </CalendarTrigger>
  </Triggers>"#;
    let installed = TaskXml::parse(&readback(&spec, &host(), triggers), &data_of(&spec)).unwrap();
    assert_eq!(installed, task);
    assert_eq!(installed.to_string(), task.to_string());
    assert_eq!(TaskXml::parse(&xml, &data_of(&spec)).unwrap(), task);
}

#[skuld::test]
fn a_schedule_that_never_fires_has_a_trigger_and_can_be_armed() {
    let spec = spec_of(User::Root, Some("0 0 31 4,6 *"));
    let task = built(&spec);
    assert!(!TaskXml::armed(&task.to_xml(false)));
    assert!(TaskXml::armed(&task.to_xml(true)));
    let armed = TaskXml::with_triggers(&task.to_xml(false), &spec).unwrap();
    assert!(TaskXml::armed(&armed));
}

// The executables =====================================================================================================

/// The sweep asks what every task runs, whether or not the task is goetia's, so the XML alone answers.
#[skuld::test]
fn executables_names_the_command_of_every_exec_action() {
    let exe = r"C:\Program Files\Goetia\goetia-shim.exe";
    let one = readback(&spec(), &host_at(exe, &["--service-host"]), DAILY_AT_0330);
    assert_eq!(TaskXml::executables(&one).unwrap(), [exe]);

    let two = one.replace(
        "</Actions>",
        r"<Exec><Command> C:\other\tool.exe </Command></Exec></Actions>",
    );
    assert_eq!(TaskXml::executables(&two).unwrap(), [exe, r"C:\other\tool.exe"]);

    let none = one
        .replace("<Exec>", "<ComHandler>")
        .replace("</Exec>", "</ComHandler>");
    assert_eq!(TaskXml::executables(&none).unwrap(), Vec::<String>::new());
}

#[skuld::test]
fn executables_of_xml_that_cannot_be_read_is_an_error() {
    assert!(TaskXml::executables("<Task><Actions>").is_err());
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
    let WindowsDefinition::Task(definition) = &task else {
        panic!("a task registered as {task:?}");
    };
    assert_eq!(definition.host(), host());
    assert_eq!(task.to_string(), built(&spec()).to_string(), "Display delegates");
    let native = DaemonSpec::from(
        crate::spec::ServiceSpec::new(crate::testing::base_fields("frpc", vec![program("frpc")])).unwrap(),
    );
    let service = registration(&native, &identity(), &|| Ok(host())).unwrap();
    assert!(matches!(service, WindowsDefinition::Service(_)), "{service:?}");
}
