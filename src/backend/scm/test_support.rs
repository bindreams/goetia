//! Fixtures shared by the SCM unit tests: byte layouts of registry string values, and a service's blob.

/// A deadline that never passes: a wait under it never expires.
pub(crate) fn unbounded() -> crate::manager::budget::Deadline {
    crate::manager::budget::Budget::Unbounded.start()
}

/// An already-spent deadline: the only kind under which a fake reports an expiry.
pub(crate) fn spent() -> crate::manager::budget::Deadline {
    crate::manager::budget::Budget::Immediate.start()
}

/// The bytes goetia stores for a string value: UTF-16LE and one closing NUL.
pub(crate) fn sz(text: &str) -> Vec<u8> {
    units_bytes(&text.encode_utf16().chain([0]).collect::<Vec<_>>())
}

/// `units` as the UTF-16LE bytes a registry value holds.
pub(crate) fn units_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}

/// The hash in the file name of a shim host, as a provider places one: SHA-256 of `b"abc"`.
pub(crate) const SHIM_HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

/// A service's blob of `protocol`, as `install` writes it for the daemon `frpc`.
#[cfg(windows)]
pub(crate) fn blob_of(protocol: crate::spec::Protocol) -> crate::blob::Blob {
    use crate::spec::{DaemonSpec, ServiceFields, ServiceSpec};
    use crate::testing::base_fields;

    crate::blob::Blob {
        schema: 1,
        version: "0.1.0".to_string(),
        spec: DaemonSpec::from(
            ServiceSpec::new(ServiceFields {
                protocol,
                ..base_fields("frpc", vec![crate::spec::test_support::program("frpc")])
            })
            .unwrap(),
        ),
    }
}

/// The task `nightly`, which runs `schedule` (`None`: nothing starts it).
#[cfg(windows)]
pub(crate) fn task_spec(schedule: Option<&str>) -> crate::spec::DaemonSpec {
    crate::testing::task("nightly", vec![crate::spec::test_support::program("job")], schedule)
}

/// The task `nightly` as `user` runs it.
#[cfg(windows)]
pub(crate) fn task_spec_as(user: crate::spec::User, schedule: Option<&str>) -> crate::spec::DaemonSpec {
    let crate::spec::DaemonSpec::Task(task) = task_spec(schedule) else {
        unreachable!("`task_spec` builds a task")
    };
    let mut fields = task.into_fields();
    fields.common.user = user;
    crate::spec::TaskSpec::new(fields)
        .map(crate::spec::DaemonSpec::from)
        .expect("a task as another user")
}

/// The shim host a task runs.
#[cfg(windows)]
pub(crate) fn task_host() -> crate::shim::HostCommand {
    crate::shim::HostCommand {
        exe: crate::spec::test_support::abs("Program Files/Goetia/goetia-shim.exe"),
        args: vec!["--service-host".to_owned()],
    }
}

/// What Task Scheduler holds for `spec` run through [`task_host`]: the XML goetia registers, its
/// triggers enabled or not, and the blob as its `Data`.
#[cfg(windows)]
pub(crate) fn registered_task(
    spec: &crate::spec::DaemonSpec,
    armed: bool,
) -> crate::backend::scm::manager::task::api::Registered {
    let definition = crate::backend::scm::task_xml::TaskXml::new(spec, task_host()).unwrap();
    crate::backend::scm::manager::task::api::Registered {
        xml: definition.to_xml(armed),
        data: crate::blob::metadata_text(spec),
        enabled: Some(true),
    }
}

/// A file at `path` whose delete is pending: its holder closed it while the returned handle was open on
/// it, so it is deleted when that handle is dropped. Windows answers an open or a delete of it with
/// access denied.
#[cfg(windows)]
pub(crate) fn delete_pending(path: &std::path::Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_DELETE_ON_CLOSE;

    let holder = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .open(path)
        .unwrap();
    let reader = std::fs::OpenOptions::new().read(true).open(path).unwrap();
    drop(holder);
    reader
}
