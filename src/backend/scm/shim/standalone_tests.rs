use super::*;
use crate::backend::scm::shim::acl::CheckPermissions;

fn shim(exe: &str) -> StandaloneShim {
    StandaloneShim::new(PathBuf::from(exe), vec!["--service-host".to_string()])
}

#[skuld::test]
fn standalone_preview_is_the_command_verbatim() {
    let command = shim(r"C:\app\app.exe").preview().unwrap();

    assert_eq!(
        command,
        HostCommand {
            exe: PathBuf::from(r"C:\app\app.exe"),
            args: vec!["--service-host".to_string()],
        }
    );
}

#[skuld::test]
fn standalone_provide_requires_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("host.exe");
    let provider = StandaloneShim::new(exe.clone(), vec![]);
    let folders = FolderCheck::new(CheckPermissions::Disable);

    match provider.provide(&folders) {
        Err(Error::Os { what, path, .. }) => {
            assert_eq!(what, "stat");
            assert_eq!(path.as_deref(), Some(exe.as_path()));
        }
        other => panic!("a missing file must fail the stat, got {other:?}"),
    }

    std::fs::write(&exe, b"host").unwrap();
    assert_eq!(provider.provide(&folders).unwrap(), provider.preview().unwrap());
}

#[skuld::test]
fn standalone_refuses_a_relative_exe() {
    let provider = shim(r"bin\app.exe");
    let folders = FolderCheck::new(CheckPermissions::Disable);

    for result in [provider.preview(), provider.provide(&folders)] {
        match result {
            Err(Error::ShimPathRelative { path }) => assert_eq!(path, PathBuf::from(r"bin\app.exe")),
            other => panic!("a relative exe must be refused, got {other:?}"),
        }
    }
}

#[skuld::test]
fn standalone_has_no_folder_whatever_its_exe() {
    for exe in [r"C:\app\host.exe", "host.exe", r"C:\"] {
        assert_eq!(shim(exe).folder().unwrap(), None, "{exe}");
    }
}

#[skuld::test]
fn standalone_sweep_removes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("host.exe");
    std::fs::write(&exe, b"host").unwrap();

    let folders = FolderCheck::new(CheckPermissions::Disable);

    StandaloneShim::new(exe.clone(), vec![]).sweep(&[], &folders).unwrap();

    assert!(exe.exists());
}
