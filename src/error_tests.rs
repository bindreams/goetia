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
        Error::NotInstalled { id: id() },
        Error::Foreign {
            id: id(),
            recovery: String::new(),
        },
        Error::Other(String::new()),
        Error::CommandFailed {
            command: String::new(),
            stderr: String::new(),
        },
        Error::NoManager {
            evidence: String::new(),
        },
    ];
    for e in &settled {
        assert!(!e.is_indeterminate(), "{e:?}");
    }
}
