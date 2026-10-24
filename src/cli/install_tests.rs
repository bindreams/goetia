use super::failure_code;
use crate::error::Error;

/// Whether the loaded job holds the old or the new definition was not established, which is the
/// question exit `4` is about; `1` would say the install determinately failed.
#[skuld::test]
fn a_loaded_job_in_doubt_exits_4() {
    let e = Error::LoadedJobInDoubt { id: "frpc".to_owned() };
    assert_eq!(failure_code(&e), 4);
}
