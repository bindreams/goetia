//! Byte layouts of registry string values and the report samples, shared by the SCM unit tests.

use crate::backend::scm::exit_status::Reported;

/// The bytes goetia stores for a string value: UTF-16LE and one closing NUL.
pub(crate) fn sz(text: &str) -> Vec<u8> {
    units_bytes(&text.encode_utf16().chain([0]).collect::<Vec<_>>())
}

/// `units` as the UTF-16LE bytes a registry value holds.
pub(crate) fn units_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}

/// One of each [`Reported`] shape. A new variant stops the match below from compiling, which is the
/// prompt to add a sample to the list; nothing forces the sample itself.
pub(crate) fn every_reported() -> Vec<Reported> {
    fn exhaustive(reported: Reported) {
        match reported {
            Reported::Clean
            | Reported::DaemonExit(_)
            | Reported::ShimFailure(_)
            | Reported::IncompleteStop
            | Reported::Refused => {}
        }
    }
    let every = vec![
        Reported::Clean,
        Reported::from_daemon_code(7),
        Reported::from_daemon_code(-1073741819),
        Reported::shim_failure(None),
        Reported::shim_failure(Some(2)),
        Reported::incomplete_stop(),
        Reported::Refused,
    ];
    every.iter().copied().for_each(exhaustive);
    every
}
