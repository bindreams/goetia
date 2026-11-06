//! The one `DaemonFields` constructor the integration suites build on.

use std::collections::BTreeMap;

use goetia::spec::{DaemonFields, Id, Kind, Restart, User};

/// The fields of a plain daemon named `id`: root, `restart: never`, `type: managed`, everything
/// else unset. Panics if `id` is not a valid `Id`.
///
/// A test that needs anything else says so with struct-update syntax, then checks the result:
/// `DaemonSpec::new(DaemonFields { env, user, ..support::spec(&id, command) })`. Built literally,
/// since the suites construct `command` and paths for this host directly.
pub fn spec(id: &str, command: Vec<String>) -> DaemonFields {
    DaemonFields {
        id: Id::try_from(id).unwrap_or_else(|e| panic!("`{id}` is not a valid Id: {e}")),
        name: id.to_string(),
        command,
        cwd: None,
        env: BTreeMap::new(),
        user: User::Root,
        restart: Restart::Never,
        restart_delay: None,
        logs: None,
        kind: Kind::Managed,
    }
}
