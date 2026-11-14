//! The metadata blob: a base64, canonical-JSON encoding of a resolved
//! `DaemonSpec`, embedded in every generated artifact. Each backend's
//! generator writes it into a `Spec=`-style directive so that
//! `generate(extract(artifact)) == artifact` is checkable without a
//! separate state store — see the crate-level design notes.
//!
//! Two properties this module exists to get right:
//!
//! - **Canonical JSON.** Struct field declaration order is not a stable
//!   wire contract: a cosmetic reorder of `DaemonSpec`'s fields must not
//!   change the bytes every already-installed service compares against.
//!   `encode` therefore serializes through an explicit key-sorting pass
//!   (`canonicalize`) rather than relying on any particular JSON map's
//!   default iteration order — that would only be *incidentally* sorted,
//!   not canonically so — and a dedicated `WireService`/`WireTask`/`WireEnvelope` set
//!   with `#[serde(rename = ...)]` on every field keeps the wire names
//!   stable even when `DaemonSpec`'s Rust field names change.
//! - **Version, distinct from schema.** `SCHEMA` changes only when the
//!   envelope's *shape* changes; `version` records the crate version that
//!   generated the artifact, so a release that changes generator output
//!   by one byte reports the installed service as stale-and-regeneratable
//!   (see `decide::Outcome::Stale`) rather than as a user hand-edit.
//!
//! `decode` builds the `DaemonSpec` through `ServiceSpec::new` or `TaskSpec::new`, so the deserialized
//! content passes every field rule: a tampered or bit-rotted artifact can
//! carry structurally valid JSON that violates them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::error::Error;
use crate::spec::{
    AccountId, CronField, DaemonFields, DaemonSpec, Id, Protocol, Restart, Schedule, ServiceFields, ServiceSpec,
    TaskFields, TaskSpec, User,
};

// Constants ===========================================================================================================

/// The envelope shape `encode`/`decode` agree on. Bumped only when the
/// JSON structure itself changes for blobs already written, never for a
/// change in generator output — that is what `version` is for. Adding a
/// member to an existing variant bumps it; adding a variant that leaves
/// existing blobs unchanged does not, so installed blobs stay readable.
pub const SCHEMA: u32 = 1;

/// The literal value every generated artifact's marker carries, byte-exact
/// per the Global Constraints (`Marker`, `Schema`, `Version`, `Spec`).
pub const MARKER: &str = "goetia";

// Blob ================================================================================================================

/// A decoded metadata blob: the schema it was written under, the crate
/// version that generated it, and the fully re-validated `DaemonSpec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub schema: u32,
    pub version: String,
    pub spec: DaemonSpec,
}

/// Encode `spec` as base64(canonical JSON), tagged with `SCHEMA` and the
/// running crate's version.
pub fn encode(spec: &DaemonSpec) -> String {
    encode_with_version(spec, crate::version())
}

/// `encode`, with the embedded version pinned explicitly rather than read
/// from `crate::version()`. Lets `blob_wire_names_are_pinned` assert
/// golden bytes that stay stable across `Cargo.toml` version bumps —
/// those aren't the wire-format drift that test guards against. `pub(crate)`
/// so a generator's own tests can build the artifact an older goetia would
/// have written, to pin `decide`'s stale-version handling against it.
pub(crate) fn encode_with_version(spec: &DaemonSpec, version: &str) -> String {
    BASE64.encode(canonical_json(SCHEMA, version, spec))
}

/// Decode a blob produced by `encode`. Re-validates every injection-gate
/// and shape invariant `DaemonSpec` requires; a structurally valid but
/// tampered or bit-rotted blob is rejected rather than trusted.
pub fn decode(encoded: &str) -> Result<Blob, Error> {
    let bytes = BASE64
        .decode(encoded)
        .map_err(|source| Error::blob_from("invalid base64", source))?;
    // The schema is read before the spec: a later schema may carry members this build refuses.
    let envelope: RawEnvelope =
        serde_json::from_slice(&bytes).map_err(|source| Error::blob_from("invalid blob JSON", source))?;

    if envelope.schema != SCHEMA {
        return Err(Error::blob(format!(
            "unsupported blob schema {found} (this build of goetia understands schema {supported})",
            found = envelope.schema,
            supported = SCHEMA,
        )));
    }

    let wire: WireDaemon =
        serde_json::from_value(envelope.spec).map_err(|source| Error::blob_from("invalid blob JSON", source))?;
    let spec = daemon_spec_from_wire(wire)?;

    Ok(Blob {
        schema: envelope.schema,
        version: envelope.version,
        spec,
    })
}

// Canonical JSON ======================================================================================================

/// Build the envelope and render it as compact, key-sorted-at-every-level
/// JSON text.
fn canonical_json(schema: u32, version: &str, spec: &DaemonSpec) -> String {
    let envelope = WireEnvelope {
        schema,
        version: version.to_string(),
        spec: WireDaemon::from(spec),
    };
    let value = serde_json::to_value(&envelope).expect("WireEnvelope's Serialize impl cannot fail");
    serde_json::to_string(&canonicalize(value)).expect("a canonicalized Value's Serialize impl cannot fail")
}

/// Recursively rebuild every JSON object with its entries inserted in
/// sorted-by-key order. This is the "key-sorted writer": it does not rely
/// on `serde_json::Map`'s default `BTreeMap` backing (an incidental
/// property that a future `preserve_order`-enabling dependency could
/// silently take away) — it sorts explicitly, so the guarantee holds
/// regardless of that backing store.
fn canonicalize(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().map(|(k, v)| (k, canonicalize(v))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

// Wire types ==========================================================================================================
//
// The wire representation of `DaemonSpec`, kept deliberately separate
// from it: every field carries an explicit `#[serde(rename = ...)]`, so a
// future rename of a `DaemonSpec` Rust field cannot silently change every
// already-written artifact's bytes.

#[derive(Debug, Clone, Serialize)]
struct WireEnvelope {
    #[serde(rename = "schema")]
    schema: u32,
    #[serde(rename = "spec")]
    spec: WireDaemon,
    #[serde(rename = "version")]
    version: String,
}

/// `WireEnvelope` as `decode` first reads it, the spec not yet interpreted.
#[derive(Deserialize)]
struct RawEnvelope {
    #[serde(rename = "schema")]
    schema: u32,
    #[serde(rename = "spec")]
    spec: Value,
    #[serde(rename = "version")]
    version: String,
}

/// The spec object of either kind. A task's carries a `type` member and a service's never does (a
/// service's own `kind` is its protocol), so the member's presence picks the struct, and each struct
/// refuses the other's members.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum WireDaemon {
    Service(WireService),
    Task(WireTask),
}

impl<'de> Deserialize<'de> for WireDaemon {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let wire = if value.get("type").is_some() {
            serde_json::from_value(value).map(WireDaemon::Task)
        } else {
            serde_json::from_value(value).map(WireDaemon::Service)
        };
        wire.map_err(D::Error::custom)
    }
}

impl From<WireService> for WireDaemon {
    fn from(service: WireService) -> Self {
        WireDaemon::Service(service)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTask {
    #[serde(rename = "id")]
    id: String,
    #[serde(rename = "name")]
    name: String,
    #[serde(rename = "command")]
    command: Vec<String>,
    #[serde(rename = "cwd")]
    cwd: Option<String>,
    #[serde(rename = "env")]
    env: BTreeMap<String, String>,
    #[serde(rename = "user")]
    user: WireUser,
    #[serde(rename = "logs")]
    logs: Option<String>,
    #[serde(rename = "type")]
    kind: WireTaskType,
    #[serde(rename = "schedule")]
    // Boxed: keeps `WireDaemon`'s variants close in size.
    schedule: Option<Box<WireSchedule>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum WireTaskType {
    #[serde(rename = "task")]
    Task,
}

/// A schedule's text and its five resolved fields. The day rule is not stored: it follows from the
/// fields' `restricted` flags.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSchedule {
    #[serde(rename = "text")]
    text: String,
    #[serde(rename = "minute")]
    minute: WireCronField,
    #[serde(rename = "hour")]
    hour: WireCronField,
    #[serde(rename = "day_of_month")]
    day_of_month: WireCronField,
    #[serde(rename = "month")]
    month: WireCronField,
    #[serde(rename = "day_of_week")]
    day_of_week: WireCronField,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCronField {
    /// Sorted.
    #[serde(rename = "values")]
    values: Vec<u8>,
    #[serde(rename = "restricted")]
    restricted: bool,
    /// What `H` resolved this field by; absent for a field with no `H` term.
    #[serde(rename = "hash", default, skip_serializing_if = "Option::is_none")]
    hash: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireService {
    #[serde(rename = "id")]
    id: String,
    #[serde(rename = "name")]
    name: String,
    #[serde(rename = "command")]
    command: Vec<String>,
    #[serde(rename = "cwd")]
    cwd: Option<String>,
    #[serde(rename = "env")]
    env: BTreeMap<String, String>,
    #[serde(rename = "user")]
    user: WireUser,
    #[serde(rename = "restart")]
    restart: WireRestart,
    #[serde(rename = "restart_delay")]
    restart_delay: Option<WireDuration>,
    #[serde(rename = "logs")]
    logs: Option<String>,
    #[serde(rename = "kind")]
    kind: WireKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
enum WireUser {
    #[serde(rename = "root")]
    Root,
    #[serde(rename = "name")]
    Name {
        #[serde(rename = "name")]
        name: String,
    },
    #[serde(rename = "uid")]
    Uid {
        #[serde(rename = "uid")]
        uid: u32,
    },
    #[serde(rename = "sid")]
    Sid {
        #[serde(rename = "sid")]
        sid: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum WireRestart {
    #[serde(rename = "never")]
    Never,
    #[serde(rename = "on-failure")]
    OnFailure,
    #[serde(rename = "always")]
    Always,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum WireKind {
    #[serde(rename = "simple")]
    Simple,
    #[serde(rename = "managed")]
    Managed,
}

/// A `std::time::Duration`, split the way `Duration`'s own fields are, so
/// the round trip through JSON is lossless to the nanosecond rather than
/// lossy the way a single milliseconds-count field would be.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct WireDuration {
    #[serde(rename = "nanos")]
    nanos: u32,
    #[serde(rename = "secs")]
    secs: u64,
}

// DaemonSpec <-> wire =================================================================================================

impl From<&DaemonSpec> for WireDaemon {
    fn from(spec: &DaemonSpec) -> Self {
        match spec {
            DaemonSpec::Service(service) => WireDaemon::Service(WireService::from(service)),
            DaemonSpec::Task(task) => WireDaemon::Task(WireTask::from(task)),
        }
    }
}

impl From<&TaskSpec> for WireTask {
    fn from(t: &TaskSpec) -> Self {
        WireTask {
            id: t.common.id.as_str().to_string(),
            name: t.common.name.clone(),
            command: t.common.command.clone(),
            cwd: t.common.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
            env: t.common.env.clone(),
            user: WireUser::from(&t.common.user),
            logs: t.common.logs.as_ref().map(|p| p.to_string_lossy().into_owned()),
            kind: WireTaskType::Task,
            schedule: t
                .schedule
                .as_ref()
                .map(|schedule| Box::new(WireSchedule::from(schedule))),
        }
    }
}

impl From<&Schedule> for WireSchedule {
    fn from(schedule: &Schedule) -> Self {
        let hashes = schedule.hashes();
        let [minute, hour, day_of_month, month, day_of_week] =
            std::array::from_fn(|i| WireCronField::new(&schedule.fields()[i], hashes[i]));
        WireSchedule {
            text: schedule.text().to_owned(),
            minute,
            hour,
            day_of_month,
            month,
            day_of_week,
        }
    }
}

impl WireCronField {
    fn new(field: &CronField, hash: Option<u64>) -> Self {
        WireCronField {
            values: field.values.iter().copied().collect(),
            restricted: field.restricted,
            hash,
        }
    }

    fn into_stored(self) -> (CronField, Option<u64>) {
        let field = CronField {
            values: self.values.into_iter().collect(),
            restricted: self.restricted,
        };
        (field, self.hash)
    }
}

impl From<&ServiceSpec> for WireService {
    fn from(s: &ServiceSpec) -> Self {
        WireService {
            id: s.common.id.as_str().to_string(),
            name: s.common.name.clone(),
            command: s.common.command.clone(),
            cwd: s.common.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
            env: s.common.env.clone(),
            user: WireUser::from(&s.common.user),
            restart: WireRestart::from(s.restart),
            restart_delay: s.restart_delay.map(|d| WireDuration {
                nanos: d.subsec_nanos(),
                secs: d.as_secs(),
            }),
            logs: s.common.logs.as_ref().map(|p| p.to_string_lossy().into_owned()),
            kind: WireKind::from(s.protocol),
        }
    }
}

impl From<&User> for WireUser {
    fn from(user: &User) -> Self {
        match user {
            User::Root => WireUser::Root,
            User::Name(name) => WireUser::Name { name: name.clone() },
            User::Id(AccountId::Uid(uid)) => WireUser::Uid { uid: *uid },
            User::Id(AccountId::Sid(sid)) => WireUser::Sid { sid: sid.clone() },
        }
    }
}

impl From<Restart> for WireRestart {
    fn from(restart: Restart) -> Self {
        match restart {
            Restart::Never => WireRestart::Never,
            Restart::OnFailure => WireRestart::OnFailure,
            Restart::Always => WireRestart::Always,
        }
    }
}

impl From<Protocol> for WireKind {
    fn from(protocol: Protocol) -> Self {
        match protocol {
            Protocol::Plain => WireKind::Simple,
            Protocol::Native => WireKind::Managed,
        }
    }
}

/// Rebuild a `DaemonSpec` from wire data. `ServiceSpec::new` and `TaskSpec::new` re-run every field
/// rule — see the module doc comment for why a structurally valid wire spec cannot be trusted as-is.
fn daemon_spec_from_wire(wire: WireDaemon) -> Result<DaemonSpec, Error> {
    match wire {
        WireDaemon::Service(service) => service_from_wire(service).map(DaemonSpec::from),
        WireDaemon::Task(task) => task_from_wire(task).map(DaemonSpec::from),
    }
}

fn service_from_wire(wire: WireService) -> Result<ServiceSpec, Error> {
    let id = Id::try_from(wire.id)?;

    let restart = match wire.restart {
        WireRestart::Never => Restart::Never,
        WireRestart::OnFailure => Restart::OnFailure,
        WireRestart::Always => Restart::Always,
    };

    let protocol = match wire.kind {
        WireKind::Simple => Protocol::Plain,
        WireKind::Managed => Protocol::Native,
    };

    let spec = ServiceSpec::new(ServiceFields {
        common: DaemonFields {
            id,
            name: wire.name,
            command: wire.command,
            cwd: wire.cwd.map(PathBuf::from),
            env: wire.env,
            user: wire.user.into(),
            logs: wire.logs.map(PathBuf::from),
        },
        restart,
        restart_delay: wire.restart_delay.as_ref().map(duration_from_wire),
        protocol,
    })?;
    // After the field rules: a blob with several defects reports the first in field order.
    if let Some(delay) = &wire.restart_delay {
        check_nanos(&spec.common.id, delay)?;
    }
    Ok(spec)
}

/// The schedule is rebuilt before the field rules run, so a blob with both defects reports the
/// schedule's.
fn task_from_wire(wire: WireTask) -> Result<TaskSpec, Error> {
    let id = Id::try_from(wire.id)?;
    let schedule = wire
        .schedule
        .map(|schedule| {
            let WireSchedule {
                text,
                minute,
                hour,
                day_of_month,
                month,
                day_of_week,
            } = *schedule;
            let fields = [minute, hour, day_of_month, month, day_of_week].map(WireCronField::into_stored);
            Schedule::from_parts(&id, text, fields)
        })
        .transpose()?;
    TaskSpec::new(TaskFields {
        common: DaemonFields {
            id,
            name: wire.name,
            command: wire.command,
            cwd: wire.cwd.map(PathBuf::from),
            env: wire.env,
            user: wire.user.into(),
            logs: wire.logs.map(PathBuf::from),
        },
        schedule,
    })
}

impl From<WireUser> for User {
    fn from(user: WireUser) -> Self {
        match user {
            WireUser::Root => User::Root,
            WireUser::Name { name } => User::Name(name),
            WireUser::Uid { uid } => User::Id(AccountId::Uid(uid)),
            WireUser::Sid { sid } => User::Id(AccountId::Sid(sid)),
        }
    }
}

/// Saturates rather than panic on `nanos >= 1e9`; [`check_nanos`] refuses that value right after.
fn duration_from_wire(wire: &WireDuration) -> Duration {
    Duration::from_secs(wire.secs).saturating_add(Duration::from_nanos(wire.nanos.into()))
}

/// `nanos >= 1e9` is `Error::Invalid` (content), not `Error::Blob` (envelope).
fn check_nanos(id: &Id, wire: &WireDuration) -> Result<(), Error> {
    if wire.nanos >= 1_000_000_000 {
        return Err(Error::Invalid {
            daemon: id.as_str().to_string(),
            message: format!(
                "restart_delay nanos {n} is not a valid sub-second fraction (must be < 1_000_000_000)",
                n = wire.nanos
            ),
            source: None,
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "blob_tests.rs"]
mod blob_tests;
