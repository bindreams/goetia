use super::failure_code;
use crate::error::Error;

/// Whether the loaded job holds the old or the new definition was not established, which is the
/// question exit `4` is about; `1` would say the install determinately failed.
#[skuld::test]
fn a_loaded_job_in_doubt_exits_4() {
    let e = Error::LoadedJobInDoubt {
        id: "frpc".to_owned(),
        restore_failed: None,
    };
    assert_eq!(failure_code(&e), 4);
}

/// The plist is written and whether the job is loaded could not be read: the same open question.
#[skuld::test]
fn an_unread_loaded_state_after_the_write_exits_4() {
    let e = Error::LoadedStateInDoubt {
        id: "frpc".to_owned(),
        detail: "launchctl print failed".to_owned(),
        restore_failed: Some("denied".to_owned()),
    };
    assert_eq!(failure_code(&e), 4);
}

/// A stale job was booted out and a concurrent writer then took the plist's place: a request was
/// sent, so the install did not determinately fail, and `1` would say it did.
#[skuld::test]
fn a_stale_job_booted_out_before_the_plist_was_taken_exits_4() {
    let e = Error::StaleJobBootedOut {
        id: "frpc".to_owned(),
        path: "/Library/Application Support/Goetia/daemons/frpc.plist".into(),
        found: "a file".to_owned(),
    };
    assert_eq!(failure_code(&e), 4);
}
