use std::path::PathBuf;
use std::time::Duration;

use super::Error;

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
        Error::Io {
            path: PathBuf::new(),
            source: std::io::Error::other("x"),
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
        Error::CommandFailed {
            command: String::new(),
            stderr: String::new(),
        },
        Error::AccountLookup { detail: String::new() },
        Error::AlreadyExists { path: PathBuf::new() },
        Error::Other(String::new()),
    ];
    for e in &settled {
        assert!(!e.is_indeterminate(), "{e:?}");
    }
}
