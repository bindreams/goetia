//! The one `DaemonFields` constructor the integration suites build on.

use std::collections::BTreeMap;

use goetia::spec::{DaemonFields, Id, Kind, Restart, User};

/// A plain root, `restart: never`, `type: managed` daemon named `id`; panics on an invalid `Id`.
/// Vary fields with struct-update syntax, then `DaemonSpec::new`.
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
